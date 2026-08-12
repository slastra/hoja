use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    App, Context, DismissEvent, Entity, EntityId, FocusHandle, Focusable, Subscription, Task,
    Window, actions, div, hsla, prelude::*, px, relative,
};
use hoja_transfer::{
    ConflictDecision, Event as JobEvent, JobHandle, JobId, JobPolicy, JobSpec, JobSummary,
    Operation, Outcome, Phase, TrashedItem, Undone,
};
use theme::ActiveTheme;

use crate::clipboard::{self, ClipboardSet};
use crate::command_palette::{self, CommandPalette};
use crate::config::{self, Settings, State};
use crate::conflict_dialog::ConflictDialog;
use crate::dir_pane::{DirPane, PaneEvent};
use crate::failure_report::{self, Failure, FailureReport};
use crate::fs;
use crate::fs::ViewSettings;
use crate::icon::Icon;
use crate::location::Location;
use crate::notifications;
use crate::open_prompt::{OpenPrompt, OpenPromptEvent};
use crate::pane_group::{PaneGroup, SplitDirection};
use crate::place_finder::{self, PlaceEvent, PlaceFinder};

actions!(
    pane,
    [
        SplitLeft,
        SplitRight,
        SplitUp,
        SplitDown,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        FocusNext,
        FocusPrevious,
        ClosePane,
        Copy,
        Cut,
        Paste,
        DismissJobs,
        PauseJobs,
        NewFolder,
        Delete,
        Undo,
    ]
);

/// A one-line status-strip message for work with no job to attach to.
/// `Problem` is coloured as an error; `Info` is not, because "nothing to undo"
/// is an answer, not a failure.
enum Notice {
    Info(String),
    Problem(String),
}

impl Notice {
    fn text(&self) -> &str {
        match self {
            Notice::Info(text) | Notice::Problem(text) => text,
        }
    }

    fn is_problem(&self) -> bool {
        matches!(self, Notice::Problem(_))
    }
}

/// How many deletions `Undo` can walk back through. Deep enough that a burst of
/// mistakes is recoverable, shallow enough that the trash entries a session
/// pins open stay bounded.
const UNDO_DEPTH: usize = 32;

const JOB_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// The one overlay a workspace shows at a time. The conflict dialog is not
/// here: it is raised by the engine rather than by the user, and must survive
/// whatever else is open.
enum Modal {
    Palette(Entity<CommandPalette>),
    Places(Entity<PlaceFinder>),
    Failures(Entity<FailureReport>),
}

impl Modal {
    fn element(&self) -> gpui::AnyElement {
        match self {
            Modal::Palette(palette) => palette.clone().into_any_element(),
            Modal::Places(finder) => finder.clone().into_any_element(),
            Modal::Failures(report) => report.clone().into_any_element(),
        }
    }
}

/// How many failures a job keeps for the report.
///
/// One per file, and a job can fail on every file it touches: copying a source
/// tree onto exFAT produced 2,619, one for each symlink the filesystem cannot
/// represent. Set well above that so the per-reason counts in the report are
/// the real ones rather than a sample: at roughly 140 bytes each this is under
/// 1.5 MB, and it is a backstop against a pathological job rather than a budget.
const MAX_RETAINED_FAILURES: usize = 10_000;

struct PendingConflict {
    job: JobId,
    dest: PathBuf,
    reply: std::sync::mpsc::Sender<ConflictDecision>,
}

/// What a copy or a cut put away.
///
/// Two kinds because members of an archive are not paths and cannot be made
/// into any: nothing outside hoja can act on them, so unlike a set of files
/// they are never mirrored to the system clipboard.
#[derive(Debug, Clone)]
enum Stash {
    Paths(ClipboardSet),
    Members {
        archive: PathBuf,
        /// The directory inside the archive they were selected from, stripped
        /// on the way out so that copying `ttf/sub` lands `sub`.
        inside: PathBuf,
        /// Member paths, files and folders alike.
        roots: Vec<String>,
        /// What the system clipboard held at the moment of the copy.
        ///
        /// Never written to, but still read at paste time: if it comes back
        /// unchanged, nothing else has claimed the clipboard since, and this
        /// selection is still the most recent thing asked for. If it comes
        /// back different, a real file was copied somewhere else afterward,
        /// and that copy wins, the same as it would over a stale copy of real
        /// files.
        baseline: Option<ClipboardSet>,
    },
}

/// Whether a freshly read external clipboard should win over an archive
/// selection that was made when the clipboard looked like `baseline`.
///
/// Pulled out on its own because it is the entire decision, and because it is
/// the one part of pasting an archive selection that a test can exercise at
/// all: the sway harness has no second application to copy from, so external
/// clipboard interop cannot be driven through it, only through this.
fn superseded(external: &Option<ClipboardSet>, baseline: &Option<ClipboardSet>) -> bool {
    match (external, baseline) {
        // Different from what was there at copy time: something real was
        // copied elsewhere since, and that is the more recent, more explicit
        // action.
        (Some(ext), Some(base)) => ext.paths != base.paths,
        // There was nothing to compare against, and now there is something:
        // the same case, since a previously empty clipboard is also a
        // baseline the read has moved past.
        (Some(_), None) => true,
        // Nothing external to prefer.
        (None, _) => false,
    }
}

/// One thing ctrl-z would take back.
///
/// Deletes and transfers in one stack, so ctrl-z means "undo the last thing I
/// did" rather than "undo the last delete, ignoring the paste in between".
enum UndoEntry {
    /// One `Delete` press. Restored in place, since a batch is a handful of
    /// renames out of the trash.
    Deleted(Vec<TrashedItem>),
    /// One transfer, by what it recorded. Given to `spawn_undo`, which gets a
    /// row of its own: taking back a copy of two hundred thousand files needs
    /// a progress bar as much as making it did.
    Transfer { label: String, records: Vec<Undone> },
}

/// A running or finished transfer job as the UI tracks it.
struct JobView {
    handle: JobHandle,
    /// Where the job writes, for refreshing affected panes on completion.
    dest_dir: PathBuf,
    /// Source parent dirs, refreshed after moves.
    src_parents: Vec<PathBuf>,
    /// A directory this job's sources were put in on its behalf, removed once
    /// it is done with them. Only an extraction has one: see `extract_into`.
    staging: Option<PathBuf>,
    done: Option<Outcome>,
    /// How many files failed, which is `failures.len()` until the cap.
    errors: usize,
    /// Example paths for the report, capped: see `MAX_RETAINED_FAILURES`.
    failures: Vec<Failure>,
    /// Every distinct reason and how many files it accounts for, uncapped.
    ///
    /// Separate from `failures` because the cap has to fall on the paths and
    /// not on the reasons. A job that fails forty thousand times on symlinks
    /// and then fills the disk keeps no path from the second reason at all, and
    /// a report grouped from the paths alone would have lost the heading with
    /// them. There are only ever a handful of distinct reasons, one per stage
    /// per errno, so nothing here needs a cap of its own.
    reasons: HashMap<String, usize>,
    /// Set when this row is undoing a transfer rather than making one, and
    /// holding the label of the one it is taking back. What it could not
    /// reverse goes back on the stack under that name, so a second ctrl-z
    /// retries it.
    undo_of: Option<String>,
    /// Whether the user has asked this job to stop, which is not whether it
    /// has: the worker parks between files, and `progress().paused` is what
    /// says it got there. Holding the request separately is what lets the gap
    /// between the two read as "pausing…" rather than as nothing happening.
    pause_requested: bool,
    /// When the transfer started, so a job short enough to have been watched
    /// does not raise a notification about it: see `notifications::NOTIFY_AFTER`.
    started: std::time::Instant,
    /// Smoothed bytes per second, and the sample it was folded from.
    rate: Option<f64>,
    last_sample: (std::time::Instant, u64),
}

/// Weight of the newest sample in the rate estimate.
///
/// A 120ms window over a tree of small files is violently bursty: one large
/// file lands and the instantaneous figure jumps by an order of magnitude, and
/// a number that jitters like that is worse than no number.
///
/// The weight sets a time constant of roughly `JOB_POLL_INTERVAL / w`, so this
/// averages over about two seconds. It was 0.25 to begin with, which is half a
/// second, and at that the reading chased every burst: 1.6, 2.1, 3.1, 1.9 GB/s
/// in consecutive frames of the same steady copy. Two seconds still follows a
/// real change of speed: crossing onto slower media, or into a directory of
/// small files: within a few frames, which is as fast as anyone can read it.
const RATE_SMOOTHING: f64 = 0.06;

/// Fold one interval's worth of bytes into the running rate estimate.
///
/// Returns the previous estimate unchanged for an interval too short to divide
/// by, which keeps a fast poll from turning a small numerator into a wild rate.
fn fold_rate(previous: Option<f64>, bytes: u64, secs: f64) -> Option<f64> {
    if secs < 0.05 {
        return previous;
    }
    let instant = bytes as f64 / secs;
    Some(match previous {
        Some(previous) => previous * (1. - RATE_SMOOTHING) + instant * RATE_SMOOTHING,
        None => instant,
    })
}

/// A number and the words that follow it, kept apart so the words can be
/// pinned. "1.5 MB" splits at its last space into "1.5" and "MB".
///
/// Together with a right-aligned number and a left-aligned unit, this is what
/// stops the unit sliding about: the number grows leftwards into its own cell
/// and the word after it never moves at all.
#[derive(Debug, Default, PartialEq, Eq)]
struct Figure {
    value: String,
    unit: String,
}

impl Figure {
    fn split(formatted: &str) -> Self {
        match formatted.rsplit_once(' ') {
            Some((value, unit)) => Figure {
                value: value.to_string(),
                unit: unit.to_string(),
            },
            // No space to split on: "…" while the scan is still running.
            None => Figure {
                value: String::new(),
                unit: formatted.to_string(),
            },
        }
    }

    /// Put back together, for a figure that has no reason to be pinned.
    fn joined(&self) -> String {
        if self.value.is_empty() {
            return self.unit.clone();
        }
        format!("{} {}", self.value, self.unit)
    }
}

/// Column widths, in the order they are laid out. Fixed, and sized for the
/// longest each can hold: four digits and a decimal point, "MB/s", "1h 59m",
/// "left". A cell sized to its content instead makes the progress bar beside it
/// grow and shrink on every repaint, since the bar is what absorbs the
/// difference, and the bar is the one thing on the strip that should hold
/// still.
const VALUE_W: f32 = 46.;
/// Sized to "MB" and no wider. Slack in this cell lands between the unit and
/// the slash, where it reads as a gap the phrase does not want; the gap that
/// matters is the one before the unit, and that one is a real gap rather than
/// leftover cell.
const SIZE_UNIT_W: f32 = 16.;
const SEP_W: f32 = 8.;
const RATE_VALUE_W: f32 = 34.;
const RATE_UNIT_W: f32 = 36.;
const LEFT_VALUE_W: f32 = 54.;
const LEFT_UNIT_W: f32 = 32.;
/// One character wide, so a split figure reads exactly like an unsplit one:
/// "381.7 MB" in two cells and "1.0 GB" in one have the same gap before the
/// unit. At half that the number and its unit ran together.
const TIGHT_GAP: f32 = 7.;
const GAP: f32 = 8.;
/// The total, which needs no growth room of its own: it is fixed for the life
/// of a job, so it is laid out as one figure held to the left of its cell,
/// starting right after the slash. Splitting it like the transferred size put
/// a number's worth of empty space between the two, and the phrase read as two
/// unrelated columns.
const TOTAL_W: f32 = 68.;
/// Advance width of one character in the metrics font at `text_xs`. Monospace,
/// so every character is this wide, which is the entire reason the metrics are
/// set in it. Only the column-width test reads it; the layout is the constants
/// above, and this is what checks they are big enough.
#[cfg(test)]
const CHAR_W: f32 = 7.3;
/// The size phrase: transferred, the slash, and the total.
const SIZE_W: f32 = VALUE_W + SIZE_UNIT_W + SEP_W + TOTAL_W + TIGHT_GAP * 3.;
/// The pause toggle's cell: a 16px icon and the padding either side of it.
///
/// Reserved whether or not there is a control in it. A finished job has
/// nothing to pause, and letting the button vanish would slide the ✕ leftwards
/// at the moment someone is reaching for it.
const PAUSE_W: f32 = 24.;
/// Reserved for the failure badge, which is wider the more files failed.
/// Holds the icon, a gap, and "2,619 failed", a five-figure count would
/// overflow it, and a job that fails ten thousand times has said enough.
const BADGE_W: f32 = 108.;
/// Everything above plus the gaps, so the states that show a sentence line up
/// with the states that show numbers.
const STATUS_W: f32 = SIZE_W + RATE_VALUE_W + RATE_UNIT_W + LEFT_VALUE_W + LEFT_UNIT_W + GAP * 4.;

/// The right-hand metrics of a job row, each number split from its unit.
///
/// A metric with nothing to say leaves its cells empty rather than shortening
/// the row.
#[derive(Debug, Default, PartialEq, Eq)]
struct Metrics {
    done: Figure,
    total: Figure,
    rate: Figure,
    left: Figure,
}

fn transfer_metrics(done: u64, total: u64, walk_complete: bool, rate: Option<f64>) -> Metrics {
    let mut metrics = Metrics {
        done: Figure::split(&fs::format_size(done)),
        total: if walk_complete {
            Figure::split(&fs::format_size(total))
        } else {
            Figure::split("…")
        },
        ..Metrics::default()
    };
    // Rate as soon as there is one; time remaining only once the scan has
    // settled a denominator to subtract from. A rate near zero means a stall,
    // and dividing by it would promise infinity: say nothing instead.
    if let Some(rate) = rate.filter(|r| *r > 1.) {
        metrics.rate = Figure::split(&format!("{}/s", fs::format_rate(rate as u64)));
        if walk_complete && total > done {
            let left = (total - done) as f64 / rate;
            let remaining = fs::format_remaining(left);
            // Never "left" on its own: the word is only meaningful attached to
            // the number it qualifies.
            if !remaining.is_empty() {
                metrics.left = Figure::split(&format!("{remaining} left"));
            }
        }
    }
    metrics
}

impl JobView {
    /// Fold the current byte count into the rate estimate.
    ///
    /// A parked job carries the sample forward without folding it. The bytes
    /// have not moved because the worker has stopped, and folding those
    /// intervals decays the reading towards zero, which takes the time
    /// remaining with it: `transfer_metrics` shows none below 1 B/s. Merely
    /// *skipping* the fold would be worse than either, since the next interval
    /// would then span the whole pause and land one enormous zero-rate sample
    /// on resume.
    fn sample(&mut self, now: std::time::Instant, bytes: u64, parked: bool) {
        if parked {
            self.last_sample = (now, bytes);
            return;
        }
        let (then, before) = self.last_sample;
        let secs = now.duration_since(then).as_secs_f64();
        let folded = fold_rate(self.rate, bytes.saturating_sub(before), secs);
        if secs >= 0.05 {
            self.last_sample = (now, bytes);
        }
        self.rate = folded;
    }

    /// How far along, from 0 to 1.
    ///
    /// Zero until the walk has settled a denominator, because a bar that ran
    /// forward and then jumped back when the total grew would be worse than
    /// one that waited.
    fn fraction(&self) -> f32 {
        use std::sync::atomic::Ordering::Relaxed;
        let progress = self.handle.progress();
        let total = progress.bytes_total.load(Relaxed);
        if progress.walk_complete.load(Relaxed) && total > 0 {
            let done = progress.bytes_done.load(Relaxed);
            return (done as f32 / total as f32).clamp(0., 1.);
        }
        if self.done.is_some() { 1. } else { 0. }
    }

    /// The same, as a whole percent. See `probe::JobProbe::percent` for why
    /// the probe rounds rather than carrying the bytes.
    fn percent(&self) -> u8 {
        (self.fraction() * 100.).round() as u8
    }

    /// Ask the worker to stop or carry on, and remember which was asked.
    fn set_paused(&mut self, paused: bool) {
        self.pause_requested = paused;
        self.handle.set_paused(paused);
    }

    /// What this job is doing.
    fn state(&self) -> JobState {
        match self.done {
            Some(Outcome::Cancelled) => return JobState::Cancelled,
            Some(_) => return JobState::Done,
            None => {}
        }
        let progress = self.handle.progress();
        // Parked beats requested, and both beat the phase: a job can be
        // stopped while scanning as easily as while transferring, and it
        // resumes into whichever it left.
        if progress.paused.load(std::sync::atomic::Ordering::Relaxed) {
            return JobState::Paused;
        }
        if self.pause_requested {
            return JobState::Pausing;
        }
        match Phase::from_u8(progress.phase.load(std::sync::atomic::Ordering::Relaxed)) {
            Phase::Scanning => JobState::Scanning,
            Phase::AwaitingConflict => JobState::Conflict,
            Phase::Flushing => JobState::Flushing,
            _ => JobState::Running,
        }
    }
}

/// What a job row is saying.
///
/// One value drives both the strip and the probe, so a test cannot pass
/// against a probe that reports something the row does not show.
///
/// `Pausing` and `Paused` are two states rather than one because pause is
/// honoured between files: a 4 GB file already in flight keeps copying after
/// the button is pressed, and a row reading "paused" over a bar that is
/// visibly still moving would be lying about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Scanning,
    Running,
    Pausing,
    Paused,
    Conflict,
    Flushing,
    Done,
    Cancelled,
}

impl JobState {
    /// The word the probe carries, and so the word a test asserts on.
    fn name(self) -> &'static str {
        match self {
            JobState::Scanning => "scanning",
            JobState::Running => "running",
            JobState::Pausing => "pausing",
            JobState::Paused => "paused",
            JobState::Conflict => "conflict",
            JobState::Flushing => "flushing",
            JobState::Done => "done",
            JobState::Cancelled => "cancelled",
        }
    }

    /// Whether there is still a worker behind this row to pause or cancel.
    fn live(self) -> bool {
        !matches!(self, JobState::Done | JobState::Cancelled)
    }

    fn parked_or_parking(self) -> bool {
        matches!(self, JobState::Paused | JobState::Pausing)
    }
}

/// Owns the pane tree and the notion of which pane is active.
///
/// Following Zed: the tree is *not* the source of truth for activation. A separate
/// `active_pane` handle plus a flat `panes` list track that, and focus drives it, a
/// pane emits `PaneEvent::Focus` on focus-in and we make it active in response. Clicking
/// a pane focuses it, so activation needs no separate click plumbing.
pub struct Workspace {
    center: PaneGroup,
    panes: Vec<Entity<DirPane>>,
    active_pane: Entity<DirPane>,
    focus_handle: FocusHandle,
    pane_subscriptions: HashMap<EntityId, Subscription>,
    /// Internal file clipboard, the source of truth for in-app paste. The
    /// system clipboard is mirrored on write and consulted on read only to
    /// interoperate with other applications.
    clipboard: Option<Stash>,
    jobs: Vec<JobView>,
    /// What ctrl-z would take back, newest last.
    ///
    /// One entry per gesture rather than per file: one `Delete` press restores
    /// a whole multi-selection, and one paste is taken back in one go however
    /// many files it moved.
    undo_stack: Vec<UndoEntry>,
    notice: Option<Notice>,
    /// One slot, so opening either closes the other. Two fields let ctrl-p on
    /// top of the palette leave both open and both rendered.
    modal: Option<Modal>,
    /// Last title pushed to the compositor, so an unchanged one is not re-sent
    /// on every frame.
    title: String,
    /// What hoja remembers between runs. Written from `save_state`, never read
    /// after startup, the panes own the live values.
    state: State,
    save_task: Option<Task<()>>,
    /// Which remembered fields are this instance's to publish: see `Dirty`.
    dirty: config::Dirty,
    /// Window-bounds and app-quit hooks; held so they stay registered.
    _lifecycle: Vec<Subscription>,
    settings_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    /// Conflicts wait here while one dialog is up; one worker blocks per job,
    /// so concurrent jobs can queue several. Tagged with the job so cancelling
    /// can drop the ones whose worker is gone.
    pending_conflicts: VecDeque<PendingConflict>,
    conflict_dialog: Option<Entity<ConflictDialog>>,
    /// The one at a time a pane can ask "open this?" about. A second archive
    /// file activated while one is up would have nowhere sensible to queue,
    /// so `confirm_extract_and_open` just refuses while this is `Some`.
    open_prompt: Option<Entity<OpenPrompt>>,
}

impl Workspace {
    pub fn new(
        start_dir: PathBuf,
        settings: Settings,
        state: State,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = config::initial_view(&settings, &state, config::newer());
        let widths = state.column_widths.clone();
        let pane = cx.new(|cx| {
            let mut pane = DirPane::new(Location::Disk(start_dir), view, window, cx);
            pane.set_column_widths(&widths);
            pane
        });
        let subscription = Self::subscribe_to_pane(&pane, window, cx);

        let mut workspace = Self {
            center: PaneGroup::new(pane.clone()),
            panes: vec![pane.clone()],
            active_pane: pane.clone(),
            focus_handle: cx.focus_handle(),
            pane_subscriptions: HashMap::from([(pane.entity_id(), subscription)]),
            clipboard: None,
            jobs: Vec::new(),
            undo_stack: Vec::new(),
            notice: None,
            modal: None,
            title: String::new(),
            state,
            save_task: None,
            dirty: config::Dirty::default(),
            settings_task: None,
            poll_task: None,
            pending_conflicts: VecDeque::new(),
            conflict_dialog: None,
            open_prompt: None,
            _lifecycle: vec![
                // `state.window` was read at startup and never written, so the
                // size was promised and never remembered. The observer fires on
                // move as well as resize; `remember_view` coalesces either way.
                cx.observe_window_bounds(window, |this, window, cx| {
                    let size = window.viewport_size();
                    let resized = config::WindowState {
                        width: size.width.into(),
                        height: size.height.into(),
                    };
                    if this.state.window != Some(resized) {
                        this.state.window = Some(resized);
                        this.dirty.window = true;
                    }
                    this.remember_settings_only(cx);
                }),
                // The throttled write is a `Task` this entity owns, so closing
                // the window drops it: cancelling the timer before it ever
                // fires. Anything toggled inside the last SAVE_DEBOUNCE window
                // went with it. Write inline here instead; there is no executor
                // left to defer to.
                //
                // Release, not `on_app_quit`: closing the last window drops the
                // window, which releases this entity, and only *then* quits the
                // app. A quit observer therefore runs when the workspace is
                // already gone, and its callback, which needs `&mut self`,
                // never fires at all. Measured: the toggle was still lost.
                cx.on_release(|this, _| this.state.save_now(this.dirty)),
            ],
        };
        workspace.watch_settings(cx);
        workspace
    }

    fn subscribe_to_pane(
        pane: &Entity<DirPane>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(pane, window, |this, pane, event, window, cx| match event {
            PaneEvent::Focus => this.set_active_pane(pane, cx),
            PaneEvent::Remove => this.remove_pane(&pane.clone(), window, cx),
            PaneEvent::ViewChanged => this.remember_view(&pane.clone(), cx),
            PaneEvent::Notice { message, problem } => {
                let notice = if *problem {
                    Notice::Problem(message.clone())
                } else {
                    Notice::Info(message.clone())
                };
                this.set_notice(Some(notice), cx)
            }
            PaneEvent::Transfer { op, sources, dest } => {
                this.spawn_transfer(*op, sources.clone(), dest.clone(), window, cx)
            }
            PaneEvent::ConfirmExtractAndOpen {
                archive,
                inside,
                member,
                name,
            } => this.confirm_extract_and_open(
                archive.clone(),
                inside.clone(),
                member.clone(),
                name.clone(),
                window,
                cx,
            ),
            PaneEvent::ExtractDrop {
                archive,
                inside,
                roots,
                dest,
            } => this.extract_into(
                archive.clone(),
                inside.clone(),
                roots.clone(),
                dest.clone(),
                window,
                cx,
            ),
        })
    }

    fn set_active_pane(&mut self, pane: &Entity<DirPane>, cx: &mut Context<Self>) {
        if &self.active_pane != pane {
            self.active_pane = pane.clone();
            self.mark_active(cx);
            cx.notify();
        }
    }

    /// Tell every pane whether it is the active one, so each can dim its own
    /// chrome and selection. Cheap: a pane that already agrees does not notify.
    fn mark_active(&self, cx: &mut Context<Self>) {
        let active = self.active_pane.entity_id();
        for pane in &self.panes {
            let is_active = pane.entity_id() == active;
            pane.update(cx, |pane, cx| pane.set_active(is_active, cx));
        }
    }

    /// Create a pane starting in `dir`, register it, and focus it.
    fn add_pane(
        &mut self,
        dir: Location,
        view: ViewSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DirPane> {
        let widths = self.state.column_widths.clone();
        let pane = cx.new(|cx| {
            let mut pane = DirPane::new(dir, view, window, cx);
            pane.set_column_widths(&widths);
            pane
        });
        let subscription = Self::subscribe_to_pane(&pane, window, cx);
        self.pane_subscriptions
            .insert(pane.entity_id(), subscription);
        self.panes.push(pane.clone());
        window.focus(&pane.focus_handle(cx), cx);
        pane
    }

    fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        // The new pane inherits the source pane's directory, so a split is a cheap way
        // to get a second view of where you already are.
        let dir = self.active_pane.read(cx).location().clone();
        // Copied at construction, so the new pane reads the directory once.
        let view = self.active_pane.read(cx).view_settings();
        let source = self.active_pane.clone();
        let new_pane = self.add_pane(dir, view, window, cx);
        self.center.split(&source, &new_pane, direction);
        #[cfg(debug_assertions)]
        eprintln!("[hoja] split {direction:?} -> {}", self.center.shape());
        cx.notify();
    }

    fn remove_pane(&mut self, pane: &Entity<DirPane>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.center.contains(pane) {
            return;
        }

        // Refuses when this is the last pane in the window.
        match self.center.remove(pane) {
            Ok(true) => {}
            _ => return,
        }

        let was_active = &self.active_pane == pane;
        self.panes.retain(|p| p != pane);
        self.pane_subscriptions.remove(&pane.entity_id());

        if was_active && let Some(fallback) = self.panes.last().cloned() {
            self.active_pane = fallback.clone();
            self.mark_active(cx);
            window.focus(&fallback.focus_handle(cx), cx);
        }
        #[cfg(debug_assertions)]
        eprintln!("[hoja] close -> {}", self.center.shape());
        cx.notify();
    }

    fn activate_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Geometric, using bounding boxes cached during the previous prepaint. Returns
        // nothing before the first paint, a chord pressed at startup does nothing.
        let Some(target) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction)
            .cloned()
        else {
            return;
        };
        window.focus(&target.focus_handle(cx), cx);
    }

    // ---- clipboard + jobs -------------------------------------------------

    fn stash_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        // Inside an archive there are no paths, and a cut is a rewrite of the
        // archive rather than a copy out of it. Copying is offered and cutting
        // is not, which is the honest pair: nothing here writes to an archive.
        if let Some((archive, inside, roots)) = self.active_pane.read(cx).selected_in_archive() {
            if roots.is_empty() {
                return;
            }
            if cut {
                self.set_notice(
                    Some(Notice::Problem(
                        "Nothing can be moved out of an archive, only copied".to_string(),
                    )),
                    cx,
                );
                return;
            }
            // Not mirrored to the system clipboard: `wl-copy` publishes
            // `file://` URIs, and there is no file for another application to
            // open at the other end of one of these. Reading it, though, is
            // what lets paste later tell "nothing has claimed the clipboard
            // since" from "something else has": see `Stash::Members::baseline`.
            cx.spawn(async move |this, cx| {
                let baseline = cx
                    .background_spawn(async move { clipboard::read_external() })
                    .await;
                let _ = this.update(cx, |this, _cx| {
                    this.clipboard = Some(Stash::Members {
                        archive,
                        inside,
                        roots,
                        baseline,
                    });
                });
            })
            .detach();
            return;
        }

        // Real paths, because this ends in `wl-copy` publishing `file://` URIs
        // to every other application on the desktop.
        let Some(paths) = self.active_pane.read(cx).selected_on_disk() else {
            self.set_notice(
                Some(Notice::Problem("There is no file here to copy".to_string())),
                cx,
            );
            return;
        };
        if paths.is_empty() {
            return;
        }
        let set = ClipboardSet { paths, cut };
        clipboard::mirror_to_system(&set);
        self.clipboard = Some(Stash::Paths(set));
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        self.stash_selection(false, cx);
    }

    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        self.stash_selection(true, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        // The transfer engine writes into a real directory, and there is not
        // one here to write into.
        let Some(dest_dir) = self.active_pane.read(cx).disk_dir().map(Path::to_path_buf) else {
            self.set_notice(
                Some(Notice::Problem(
                    "There is nowhere here to paste into".to_string(),
                )),
                cx,
            );
            return;
        };
        let internal = self.clipboard.clone();

        // The external read shells out to wl-paste, which blocks on the current
        // clipboard owner, so it runs on the background executor. External
        // content wins when it looks like a newer, different copy; the
        // internal clipboard otherwise, whichever kind that is.
        let task = cx.spawn_in(window, async move |this, cx| {
            let external = cx
                .background_spawn(async move { clipboard::read_external() })
                .await;

            match internal {
                Some(Stash::Members {
                    archive,
                    inside,
                    roots,
                    baseline,
                }) => {
                    if superseded(&external, &baseline) {
                        // Something real was copied elsewhere after the
                        // archive selection was made, and that is the more
                        // recent, more explicit action.
                        let Some(set) = external else { return };
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.spawn_transfer(
                                if set.cut {
                                    Operation::Move
                                } else {
                                    Operation::Copy
                                },
                                set.paths.clone(),
                                dest_dir,
                                window,
                                cx,
                            );
                        });
                    } else {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.extract_into(archive, inside, roots, dest_dir, window, cx);
                        });
                    }
                }
                other => {
                    let internal = match other {
                        Some(Stash::Paths(set)) => Some(set),
                        _ => None,
                    };
                    let set = match (external, internal) {
                        // Our own mirror read back through wl-paste is not "another app".
                        (Some(ext), Some(int)) if ext.paths == int.paths => Some(int),
                        (Some(ext), _) => Some(ext),
                        (None, int) => int,
                    };
                    let Some(set) = set else { return };

                    let _ = this.update_in(cx, |this, window, cx| {
                        this.spawn_transfer(
                            if set.cut {
                                Operation::Move
                            } else {
                                Operation::Copy
                            },
                            set.paths.clone(),
                            dest_dir,
                            window,
                            cx,
                        );
                        // A cut pastes once; a copy pastes repeatedly.
                        if set.cut {
                            this.clipboard = None;
                        }
                    });
                }
            }
        });
        task.detach();
    }

    /// Copy things out of an archive into `dest_dir`.
    ///
    /// Two steps, deliberately. The members are extracted into a hidden
    /// directory **inside the destination**, and the results are then handed to
    /// the transfer engine as a move. Being on the destination's own filesystem
    /// by construction, that move is a rename per item rather than a second
    /// copy, and it buys the whole of what the engine already does well: the
    /// conflict dialog, keep-both naming, the progress strip, the failure
    /// report and cancellation. Extracting straight into the destination would
    /// have meant writing all of that again, worse.
    fn extract_into(
        &mut self,
        archive: PathBuf,
        inside: PathBuf,
        roots: Vec<String>,
        dest_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inside the destination, so the move at the end is a rename, and
        // named the way the engine names a file it is part-way through
        // writing: `is_partial_name` already keeps those out of every listing,
        // and a staging directory is exactly that sort of thing.
        let temp = hoja_transfer::partial_path(&dest_dir.join("extract"));
        let label = match roots.len() {
            1 => "1 item".to_string(),
            n => format!("{} items", notifications::count(n as u64)),
        };
        self.set_notice(Some(Notice::Info(format!("Extracting {label}…"))), cx);

        let cancel = crate::archive::Cancel::new();
        let task = cx.spawn_in(window, async move |this, cx| {
            let out = temp.clone();
            let extracted = cx
                .background_spawn(async move {
                    std::fs::create_dir(&out)?;
                    let failures = crate::archive::extract(
                        &archive,
                        &inside,
                        &roots,
                        &out,
                        &crate::archive::Progress::default(),
                        &cancel,
                    )?;
                    // What actually landed, which is what the engine is given.
                    // Reading the directory rather than deriving it from the
                    // selection: a member that failed left nothing behind, and
                    // handing the engine a path to nothing would turn one
                    // failure into two.
                    let sources: Vec<PathBuf> = std::fs::read_dir(&out)?
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect();
                    anyhow::Ok((sources, failures))
                })
                .await;

            let _ = this.update_in(cx, |this, window, cx| match extracted {
                Ok((sources, failures)) => {
                    if !failures.is_empty() {
                        this.set_notice(
                            Some(Notice::Problem(format!(
                                "{} could not be extracted",
                                match failures.len() {
                                    1 => "1 file".to_string(),
                                    n => format!("{} files", notifications::count(n as u64)),
                                }
                            ))),
                            cx,
                        );
                    } else {
                        this.set_notice(None, cx);
                    }
                    if sources.is_empty() {
                        let _ = std::fs::remove_dir_all(&temp);
                        return;
                    }
                    this.spawn_transfer_from(
                        Operation::Move,
                        sources,
                        dest_dir,
                        Some(temp),
                        window,
                        cx,
                    );
                }
                Err(err) => {
                    let _ = std::fs::remove_dir_all(&temp);
                    this.set_notice(Some(Notice::Problem(err.to_string())), cx);
                }
            });
        });
        task.detach();
    }

    /// Ask before opening a file that lives inside an archive.
    ///
    /// One at a time: a second file activated while the dialog is up is
    /// dropped rather than queued, the same call `maybe_show_conflict` makes
    /// for conflicts, and for the same reason. Nothing here is destructive
    /// enough to be worth remembering past this session.
    fn confirm_extract_and_open(
        &mut self,
        archive: PathBuf,
        inside: PathBuf,
        member: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_prompt.is_some() {
            return;
        }

        let dialog = cx.new(|cx| OpenPrompt::new(name, window, cx));
        cx.subscribe_in(&dialog, window, move |this, _, event, window, cx| {
            this.open_prompt = None;
            window.focus(&this.active_pane.focus_handle(cx), cx);
            if matches!(event, OpenPromptEvent::Confirmed) {
                this.extract_and_open(archive.clone(), inside.clone(), member.clone(), window, cx);
            }
            cx.notify();
        })
        .detach();

        window.focus(&dialog.focus_handle(cx), cx);
        self.open_prompt = Some(dialog);
        cx.notify();
    }

    /// Extract one file from an archive into a fresh directory under the
    /// system temp dir, then hand it to the desktop's default opener.
    ///
    /// Nowhere near the archive and nowhere near anything the pane is
    /// showing: this is a throwaway copy, not a destination, and `OpenPrompt`
    /// already said so before this ran.
    fn extract_and_open(
        &mut self,
        archive: PathBuf,
        inside: PathBuf,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        static SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = std::env::temp_dir().join(format!("hoja-open-{}-{n}", std::process::id()));

        self.set_notice(Some(Notice::Info("Extracting…".to_string())), cx);

        let cancel = crate::archive::Cancel::new();
        let roots = vec![member];
        let task = cx.spawn_in(window, async move |this, cx| {
            let out = dest.clone();
            let result = cx
                .background_spawn(async move {
                    std::fs::create_dir_all(&out)?;
                    let failures = crate::archive::extract(
                        &archive,
                        &inside,
                        &roots,
                        &out,
                        &crate::archive::Progress::default(),
                        &cancel,
                    )?;
                    // What actually landed, the same reasoning `extract_into`
                    // follows: a member that failed left nothing behind, so
                    // this is what there is to open rather than what was asked
                    // for.
                    let opened: Vec<PathBuf> = std::fs::read_dir(&out)?
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect();
                    anyhow::Ok((opened, failures))
                })
                .await;

            let _ = this.update(cx, |this, cx| match result {
                Ok((opened, failures)) if opened.is_empty() || !failures.is_empty() => {
                    let _ = std::fs::remove_dir_all(&dest);
                    this.set_notice(
                        Some(Notice::Problem("Could not extract that file".to_string())),
                        cx,
                    );
                }
                Ok((opened, _)) => {
                    this.set_notice(None, cx);
                    if let Some(path) = opened.first()
                        && let Err(err) = crate::opener::open(path)
                    {
                        this.set_notice(Some(Notice::Problem(err.to_string())), cx);
                    }
                }
                Err(err) => {
                    let _ = std::fs::remove_dir_all(&dest);
                    this.set_notice(Some(Notice::Problem(err.to_string())), cx);
                }
            });
        });
        task.detach();
    }

    fn spawn_transfer(
        &mut self,
        op: Operation,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.spawn_transfer_from(op, sources, dest_dir, None, window, cx);
    }

    /// `staging` is a directory this job's sources were put in on its behalf,
    /// removed once the job is done with them. Only an extraction has one.
    fn spawn_transfer_from(
        &mut self,
        op: Operation,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        staging: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let src_parents: Vec<PathBuf> = sources
            .iter()
            .filter_map(|p| p.parent().map(std::path::Path::to_path_buf))
            .collect();

        let spec = JobSpec {
            op,
            sources,
            dest_dir: dest_dir.clone(),
            policy: JobPolicy::default(),
        };
        match hoja_transfer::spawn_job(spec) {
            Ok(handle) => {
                self.jobs.push(JobView {
                    handle,
                    dest_dir,
                    src_parents,
                    staging,
                    done: None,
                    errors: 0,
                    failures: Vec::new(),
                    reasons: HashMap::new(),
                    undo_of: None,
                    pause_requested: false,
                    started: std::time::Instant::now(),
                    rate: None,
                    last_sample: (std::time::Instant::now(), 0),
                });
                self.ensure_polling(window, cx);
                cx.notify();
            }
            // Drag-and-drop greys an illegal target, but paste has no such
            // pre-check, so this is the only way the refusal is ever seen.
            Err(err) => {
                // Nothing is going to move out of it now.
                if let Some(staging) = &staging {
                    let _ = std::fs::remove_dir_all(staging);
                }
                self.set_notice(Some(Notice::Problem(err.to_string())), cx)
            }
        }
    }

    /// One poll loop for all jobs, alive only while jobs exist. 120ms matches
    /// the "sample progress on a timer" design: frame-driven polling would
    /// couple a ~8Hz progress bar to vsync for no benefit. Spawned in the
    /// window so conflict prompts can be raised from inside the poll.
    fn ensure_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(JOB_POLL_INTERVAL).await;
                let keep_going = this
                    .update_in(cx, |this, window, cx| {
                        this.poll_jobs(window, cx);
                        !this.jobs.is_empty()
                    })
                    .unwrap_or(false);
                if !keep_going {
                    let _ = this.update(cx, |this, _| this.poll_task = None);
                    break;
                }
            }
        }));
    }

    fn poll_jobs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut finished_dirs: Vec<PathBuf> = Vec::new();
        let mut finished_jobs: Vec<JobId> = Vec::new();
        let mut announce: Vec<(String, PathBuf, std::time::Duration, JobSummary)> = Vec::new();
        // Collected rather than pushed in place: the loop below holds a
        // mutable borrow of `self.jobs`, and the stack lives beside it.
        let mut remember: Vec<UndoEntry> = Vec::new();

        let now = std::time::Instant::now();
        for job in &mut self.jobs {
            let job_id = job.handle.id();
            if job.done.is_none() {
                let (bytes, parked) = {
                    let progress = job.handle.progress();
                    (
                        progress
                            .bytes_done
                            .load(std::sync::atomic::Ordering::Relaxed),
                        progress.paused.load(std::sync::atomic::Ordering::Relaxed),
                    )
                };
                job.sample(now, bytes, parked);
            }
            while let Some(event) = job.handle.try_recv_event() {
                match event {
                    JobEvent::Conflict { dest, reply, .. } => {
                        self.pending_conflicts.push_back(PendingConflict {
                            job: job_id,
                            dest,
                            reply,
                        });
                    }
                    JobEvent::FileError { path, error } => {
                        job.errors += 1;
                        let reason = error.to_string();
                        *job.reasons
                            .entry(failure_report::tidy(&reason).to_string())
                            .or_default() += 1;
                        if job.failures.len() < MAX_RETAINED_FAILURES {
                            job.failures.push(Failure { path, reason });
                        }
                    }
                    JobEvent::Warning { .. } => {}
                    JobEvent::Done(summary) => {
                        job.done = Some(summary.outcome);
                        finished_jobs.push(job_id);
                        // A move empties it, but a cancellation or a refused
                        // conflict can leave things behind, and none of them
                        // belong to anyone now. `remove_dir_all` rather than
                        // `remove_dir` for exactly that.
                        if let Some(staging) = job.staging.take() {
                            let _ = std::fs::remove_dir_all(staging);
                        }
                        // The summary is the authoritative list, it holds
                        // failures raised before the strip started polling, and
                        // the walk's own, which arrive as no `FileError` at
                        // all. Rebuilding from it cannot double-count.
                        job.errors = summary.errors.len();
                        job.reasons.clear();
                        for (_, error) in &summary.errors {
                            *job.reasons
                                .entry(failure_report::tidy(&error.to_string()).to_string())
                                .or_default() += 1;
                        }
                        job.failures = summary
                            .errors
                            .iter()
                            .take(MAX_RETAINED_FAILURES)
                            .map(|(path, error)| Failure {
                                path: path.clone(),
                                reason: error.to_string(),
                            })
                            .collect();
                        finished_dirs.push(job.dest_dir.clone());
                        finished_dirs.extend(job.src_parents.iter().cloned());

                        // What ctrl-z would take back next. An undo hands back
                        // what it could *not* reverse, under the name of the
                        // transfer it was undoing, so a second press retries
                        // exactly those; a transfer hands back what it did.
                        if !summary.undone.is_empty() {
                            remember.push(match &job.undo_of {
                                Some(label) => UndoEntry::Transfer {
                                    label: label.clone(),
                                    records: summary.undone.clone(),
                                },
                                None => UndoEntry::Transfer {
                                    label: job.handle.label().to_string(),
                                    records: summary.undone.clone(),
                                },
                            });
                        }

                        announce.push((
                            job.handle.label().to_string(),
                            job.dest_dir.clone(),
                            job.started.elapsed(),
                            summary,
                        ));
                    }
                }
            }
        }

        for entry in remember {
            self.remember_undo(entry);
        }

        // Clean finished jobs disappear on their own; failed ones persist until
        // dismissed so the error is actually seen.
        self.jobs
            .retain(|job| !(job.done.is_some() && job.errors == 0));

        for job in finished_jobs {
            self.purge_conflicts(job);
        }
        self.maybe_show_conflict(window, cx);

        for (label, dest, elapsed, summary) in announce {
            Self::announce_finished(&label, &dest, elapsed, &summary, cx);
        }

        self.refresh_dirs(&finished_dirs, cx);
        cx.notify();
    }

    /// Tell the desktop a transfer finished, when it is worth telling.
    ///
    /// Not for a job you watched happen, and not for one you cancelled, you
    /// already know. Errors always announce, however brief the job: the strip
    /// keeps a failed transfer around until it is dismissed, but only a
    /// notification reaches you when hoja is not the window you are looking at.
    fn announce_finished(
        label: &str,
        dest: &Path,
        elapsed: std::time::Duration,
        summary: &JobSummary,
        cx: &App,
    ) {
        if !notifications::worth_announcing(summary.outcome, summary.errors.len(), elapsed) {
            return;
        }
        let failed = !summary.errors.is_empty();

        let where_to = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dest.display().to_string());
        let files = notifications::count(summary.files_copied);
        let body = if failed {
            format!(
                "{files} of {} to {where_to}, {} failed",
                notifications::count(summary.files_copied + summary.errors.len() as u64),
                notifications::count(summary.errors.len() as u64)
            )
        } else {
            format!("{files} files to {where_to}")
        };
        notifications::transfer_finished(label.to_string(), body, failed, cx);
    }

    /// Show the next queued conflict in the themed dialog. The engine's worker
    /// stays blocked (cancel-aware) until the dialog answers into `reply`; the
    /// UI thread never blocks.
    fn maybe_show_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conflict_dialog.is_some() {
            return;
        }
        let Some(pending) = self.pending_conflicts.pop_front() else {
            return;
        };

        let dialog = cx.new(|cx| ConflictDialog::new(&pending.dest, pending.reply, window, cx));
        cx.subscribe_in(&dialog, window, |this, _, _: &DismissEvent, window, cx| {
            this.conflict_dialog = None;
            window.focus(&this.active_pane.focus_handle(cx), cx);
            this.maybe_show_conflict(window, cx);
            cx.notify();
        })
        .detach();

        window.focus(&dialog.focus_handle(cx), cx);
        self.conflict_dialog = Some(dialog);
        cx.notify();
    }

    fn new_folder(&mut self, _: &NewFolder, _window: &mut Window, cx: &mut Context<Self>) {
        // `create_dir` needs somewhere real to create it.
        let Some(dir) = self.active_pane.read(cx).disk_dir().map(Path::to_path_buf) else {
            self.set_notice(
                Some(Notice::Problem(
                    "There is nowhere here to make a folder".to_string(),
                )),
                cx,
            );
            return;
        };
        let mut candidate = dir.join("New Folder");
        let mut n = 1;
        while candidate.exists() {
            n += 1;
            candidate = dir.join(format!("New Folder {n}"));
        }
        match std::fs::create_dir(&candidate) {
            Ok(()) => self.active_pane.update(cx, |pane, cx| pane.refresh(cx)),
            Err(err) => eprintln!("[hoja] new folder failed: {err}"),
        }
    }

    /// Delete the active pane's selection by moving it to the trash directory,
    /// which is what makes `Undo` possible, an unlinked file cannot come back.
    ///
    /// There is deliberately no confirmation dialog: undo is the safety net, and
    /// a dialog that is always dismissed protects nobody.
    fn delete(&mut self, _: &Delete, cx: &mut Context<Self>) {
        // `trash` moves a real file into a real trash directory, so a row that
        // is not one has nothing to move.
        let Some(paths) = self.active_pane.read(cx).selected_on_disk() else {
            self.set_notice(
                Some(Notice::Problem(
                    "There is no file here to delete".to_string(),
                )),
                cx,
            );
            return;
        };
        if paths.is_empty() {
            return;
        }
        let parents = parent_dirs(&paths);
        let pane = self.active_pane.clone();

        // Each item is a rename, so this is fast, but "fast" is a property of
        // the filesystem, and a stalled network mount must not take the UI with
        // it.
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| hoja_transfer::trash(&path).map_err(|err| (path, err)))
                        .collect::<Vec<_>>()
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                let mut trashed = Vec::new();
                let mut failures = Vec::new();
                for result in results {
                    match result {
                        Ok(item) => trashed.push(item),
                        Err(failure) => failures.push(failure),
                    }
                }

                if !trashed.is_empty() {
                    let names: Vec<PathBuf> =
                        trashed.iter().map(|item| item.original.clone()).collect();
                    pane.update(cx, |pane, cx| pane.select_after_removal(&names, cx));
                    this.remember_undo(UndoEntry::Deleted(trashed));
                }
                this.set_notice(delete_failure_notice(&failures), cx);
                this.refresh_dirs(&parents, cx);
            });
        })
        .detach();
    }

    /// Push something onto the undo stack, oldest off the end.
    fn remember_undo(&mut self, entry: UndoEntry) {
        self.undo_stack.push(entry);
        if self.undo_stack.len() > UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
    }

    /// Take back the last thing that was done.
    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        // Popped before the work starts, not after it finishes, so a second
        // press walks a second entry back rather than racing the first.
        let Some(entry) = self.undo_stack.pop() else {
            self.set_notice(Some(Notice::Info("Nothing to undo".to_string())), cx);
            return;
        };
        match entry {
            UndoEntry::Deleted(batch) => self.undo_delete(batch, cx),
            UndoEntry::Transfer { label, records } => {
                self.undo_transfer(label, records, window, cx)
            }
        }
    }

    /// Take a transfer back, as a job of its own.
    fn undo_transfer(
        &mut self,
        label: String,
        records: Vec<Undone>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every directory the records touch, so the panes showing them refresh
        // when it finishes. Both ends: a move put things somewhere and took
        // them from somewhere else.
        let mut touched: Vec<PathBuf> = Vec::new();
        for record in &records {
            match record {
                Undone::Renamed { from, to, .. } => {
                    touched.push(from.clone());
                    touched.push(to.clone());
                }
                Undone::Created { path, from, .. } => {
                    touched.push(path.clone());
                    touched.extend(from.clone());
                }
                Undone::CreatedDir(path) | Undone::Lost(path) => touched.push(path.clone()),
                Undone::Displaced(item) => touched.push(item.original.clone()),
            }
        }
        let parents = parent_dirs(&touched);

        match hoja_transfer::spawn_undo(label.clone(), records) {
            Ok(handle) => {
                self.jobs.push(JobView {
                    handle,
                    dest_dir: parents.first().cloned().unwrap_or_default(),
                    src_parents: parents,
                    staging: None,
                    done: None,
                    errors: 0,
                    failures: Vec::new(),
                    reasons: HashMap::new(),
                    undo_of: Some(label),
                    pause_requested: false,
                    started: std::time::Instant::now(),
                    rate: None,
                    last_sample: (std::time::Instant::now(), 0),
                });
                self.ensure_polling(window, cx);
                cx.notify();
            }
            Err(err) => {
                // The records go back: nothing was taken back, so the entry is
                // still owed.
                self.set_notice(
                    Some(Notice::Problem(format!("Could not undo {label}: {err}"))),
                    cx,
                );
            }
        }
    }

    /// Put the most recent deletion back.
    fn undo_delete(&mut self, batch: Vec<TrashedItem>, cx: &mut Context<Self>) {
        let parents = parent_dirs(&batch.iter().map(|i| i.original.clone()).collect::<Vec<_>>());

        cx.spawn(async move |this, cx| {
            let (restored, failures) = cx
                .background_spawn(async move {
                    let mut restored = Vec::new();
                    let mut failures = Vec::new();
                    for item in batch {
                        match hoja_transfer::restore(&item) {
                            Ok(()) => restored.push(item.original),
                            Err(err) => failures.push((item, err)),
                        }
                    }
                    (restored, failures)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                // Anything that could not go back stays on the stack, so a
                // second ctrl-z retries it rather than losing the record.
                if !failures.is_empty() {
                    let (first_path, first_err) = failures
                        .first()
                        .map(|(item, err)| (item.original.clone(), err.to_string()))
                        .unwrap();
                    this.remember_undo(UndoEntry::Deleted(
                        failures.into_iter().map(|(item, _)| item).collect(),
                    ));
                    this.set_notice(
                        Some(Notice::Problem(format!(
                            "Could not restore {}: {first_err}",
                            file_label(&first_path)
                        ))),
                        cx,
                    );
                } else {
                    this.set_notice(None, cx);
                }
                if !restored.is_empty() {
                    this.select_in_panes(&restored, cx);
                }
                this.refresh_dirs(&parents, cx);
            });
        })
        .detach();
    }

    /// Set or clear the strip's message. Clearing is a no-op when there is
    /// nothing to clear, so routine work does not repaint the strip.
    fn set_notice(&mut self, notice: Option<Notice>, cx: &mut Context<Self>) {
        if notice.is_none() && self.notice.is_none() {
            return;
        }
        self.notice = notice;
        cx.notify();
    }

    /// Re-list every pane showing one of `dirs`.
    fn refresh_dirs(&mut self, dirs: &[PathBuf], cx: &mut Context<Self>) {
        for pane in &self.panes {
            // A pane inside an archive shows no directory, so it matches none
            // of these and is left alone.
            let Some(pane_dir) = pane.read(cx).disk_dir().map(Path::to_path_buf) else {
                continue;
            };
            if dirs.contains(&pane_dir) {
                pane.update(cx, |pane, cx| pane.refresh(cx));
            }
        }
        cx.notify();
    }

    /// Put the selection back on restored items, in whichever pane shows them.
    fn select_in_panes(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for pane in &self.panes {
            // Restored items land on the disk, so only a pane showing the disk
            // can be showing them.
            let Some(pane_dir) = pane.read(cx).disk_dir().map(Path::to_path_buf) else {
                continue;
            };
            let landed: Vec<PathBuf> = paths
                .iter()
                .filter(|p| p.parent() == Some(pane_dir.as_path()))
                .cloned()
                .collect();
            if !landed.is_empty() {
                pane.update(cx, |pane, _| pane.select_on_next_load(landed));
            }
        }
    }

    /// `ctrl-shift-p`. Pressing it again closes, matching every editor.
    fn toggle_palette(
        &mut self,
        _: &command_palette::Toggle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.modal, Some(Modal::Palette(_))) {
            self.close_modal(window, cx);
            return;
        }

        // Any *other* modal (the place finder) still holds focus here, and
        // `available_actions` resolves against whatever is focused rather than
        // against the handle it is passed. Opening the palette over the finder
        // therefore enumerated the finder's dispatch path, and every pane
        // command was missing from the list. Closing it first hands focus back
        // to the pane.
        if self.modal.is_some() {
            self.close_modal(window, cx);
        }

        // Captured before the palette takes focus: the action list, the key
        // bindings shown, and where a confirmed action dispatches all hang off
        // this handle.
        let origin = self.active_pane.focus_handle(cx);
        let palette = cx.new(|cx| CommandPalette::new(origin, window, cx));

        cx.subscribe_in(&palette, window, |this, _, _: &DismissEvent, window, cx| {
            this.close_modal(window, cx)
        })
        .detach();

        // The query field takes focus, not the palette shell: typing has to
        // reach the editor, and the palette's own bindings still fire because
        // the field is inside its dispatch path.
        let query_focus = palette.read(cx).query_focus(cx);
        window.focus(&query_focus, cx);
        self.modal = Some(Modal::Palette(palette));
        cx.notify();
    }

    /// `ctrl-p`. Jump to home, a bookmark, or an attached volume.
    fn toggle_places(
        &mut self,
        _: &place_finder::Toggle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.modal, Some(Modal::Places(_))) {
            self.close_modal(window, cx);
            return;
        }

        let finder = cx.new(|cx| PlaceFinder::new(window, cx));

        cx.subscribe_in(&finder, window, |this, _, _: &DismissEvent, window, cx| {
            this.close_modal(window, cx)
        })
        .detach();
        // Mounting outlives the finder's own dismissal, so this subscription
        // has to survive it: the entity is kept alive by the closure below
        // until it reports back.
        cx.subscribe_in(
            &finder,
            window,
            |this, _, event: &PlaceEvent, window, cx| match event {
                PlaceEvent::Open(path) => {
                    this.active_pane.update(cx, |pane, cx| {
                        pane.navigate_to(Location::Disk(path.clone()), cx)
                    });
                    window.focus(&this.active_pane.focus_handle(cx), cx);
                }
                PlaceEvent::Mount { device, label } => {
                    this.mount_and_open(device.clone(), label.clone(), cx)
                }
                PlaceEvent::Unmount {
                    device,
                    label,
                    mount,
                } => this.unmount(device.clone(), label.clone(), mount.clone(), cx),
            },
        )
        .detach();

        let query_focus = finder.read(cx).query_focus(cx);
        window.focus(&query_focus, cx);
        self.modal = Some(Modal::Places(finder));
        cx.notify();
    }

    /// Mount a volume, then point the active pane at it.
    ///
    /// Runs here rather than in the finder because the finder dismisses the
    /// moment a place is chosen. `udisksctl` waits on udisks and polkit and can
    /// take seconds or raise an authentication prompt, so the call itself goes
    /// to the background and the strip says what is happening meanwhile.
    fn mount_and_open(&mut self, device: PathBuf, label: String, cx: &mut Context<Self>) {
        self.set_notice(Some(Notice::Info(format!("Mounting {label}…"))), cx);

        cx.spawn(async move |this, cx| {
            let mounted = cx
                .background_spawn(async move { crate::places::mount(&device) })
                .await;
            let _ = this.update(cx, |this, cx| match mounted {
                Ok(path) => {
                    this.set_notice(None, cx);
                    this.active_pane
                        .update(cx, |pane, cx| pane.navigate_to(Location::Disk(path), cx));
                }
                Err(err) => this.set_notice(
                    Some(Notice::Problem(format!("Could not mount {label}: {err}"))),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Unmount a volume, then bounce any pane that was sitting under it.
    ///
    /// The other half of `mount_and_open`, for the same reason: `udisksctl`
    /// can take a moment, so the call goes to the background and the strip
    /// says what is happening meanwhile.
    fn unmount(&mut self, device: PathBuf, label: String, mount: PathBuf, cx: &mut Context<Self>) {
        self.set_notice(Some(Notice::Info(format!("Ejecting {label}…"))), cx);

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { crate::places::unmount(&device) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.set_notice(None, cx);
                    // The directory under every one of these just stopped
                    // existing. `refresh` re-reads it, and `reload_inner`'s own
                    // "gone" fallback (the same one a deleted folder hits)
                    // walks each pane up to the nearest ancestor still there,
                    // which is the containing directory once the mount point
                    // itself is gone too.
                    for pane in &this.panes {
                        let under = pane
                            .read(cx)
                            .disk_dir()
                            .is_some_and(|dir| dir.starts_with(&mount));
                        if under {
                            pane.update(cx, |pane, cx| pane.refresh(cx));
                        }
                    }
                }
                Err(err) => this.set_notice(
                    Some(Notice::Problem(format!("Could not eject {label}: {err}"))),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Re-read `settings.json` when it changes and apply it over what was
    /// remembered.
    ///
    /// This is what makes the two files coherent. State wins at startup, so a
    /// toggle survives a restart; but an edit to the hand-written file happens
    /// *later* than that toggle, so it takes effect and is then itself
    /// remembered. Whichever answer is most recent is the one in force, which
    /// is the only rule that does not surprise.
    fn watch_settings(&mut self, cx: &mut Context<Self>) {
        let Some(path) = config::settings_file() else {
            return;
        };
        let Some(dir) = path.parent().map(std::path::Path::to_path_buf) else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                eprintln!("[hoja] settings watcher unavailable: {err}");
                return;
            }
        };
        // The directory, not the file: an editor that writes by rename would
        // otherwise leave the watch pointing at a deleted inode.
        use notify::Watcher as _;
        if watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .is_err()
        {
            return;
        }

        self.settings_task = Some(cx.spawn(async move |this, cx| {
            let _watcher = watcher;
            loop {
                cx.background_executor().timer(config::SAVE_DEBOUNCE).await;
                if !config::drain_changes(&rx) {
                    continue;
                }
                if this
                    .update(cx, |this, cx| {
                        this.apply_settings(config::Settings::load(), cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Push a freshly read settings file over every pane, and remember it.
    fn apply_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        if let Some(name) = &settings.theme
            && let Err(err) = crate::theming::apply(name, cx)
        {
            eprintln!("[hoja] theme {name:?} not available: {err}");
        }

        // The file was just written, so it is the newest answer and wins where
        // it has one. Where it has none, as in a file that only sets a theme,
        // what was remembered still stands, which is why the real state goes in
        // here and not an empty one. Passing `State::default()` reset every
        // pane to the compiled defaults on any settings edit, and then wrote
        // the reset out through `remember_view` below.
        let view = config::initial_view(&settings, &self.state, config::Winner::Settings);
        for pane in &self.panes {
            pane.update(cx, |pane, cx| pane.set_view_settings(view, cx));
        }
        // Every pane just took the same `view`, so any of them says the same
        // thing; the widths are nobody's to republish here.
        self.remember_settings_only(cx);
        cx.notify();
    }

    /// Record what `pane` is showing, and write it out shortly.
    ///
    /// The pane is passed in rather than taken from `active_pane`, because the
    /// two are not always the same one: a column dragged in a pane that does
    /// not hold focus reported the change from the pane that did, and saved
    /// widths nobody had touched.
    ///
    /// Debounced because a column drag changes this on every frame; without it
    /// a single resize would be a few hundred writes.
    pub fn remember_view(&mut self, pane: &Entity<DirPane>, cx: &mut Context<Self>) {
        let pane = pane.read(cx);
        let (view, widths) = (pane.view_settings(), pane.column_widths());
        self.remember(view, Some(widths), cx);
    }

    /// Record the view settings without republishing anyone's column widths.
    ///
    /// For a change that did not come from a pane: the window moving, or the
    /// settings file being saved. Those used to go through `remember_view` with
    /// the active pane, which reassigned `state.column_widths` from whichever
    /// pane happened to hold focus. A width dragged in the *other* pane was
    /// already recorded and waiting behind the save throttle, so moving the
    /// window before it landed replaced it, and the next start handed the
    /// focused pane's widths to every pane.
    fn remember_settings_only(&mut self, cx: &mut Context<Self>) {
        let view = self.active_pane.read(cx).view_settings();
        self.remember(view, None, cx);
    }

    fn remember(
        &mut self,
        view: ViewSettings,
        column_widths: Option<std::collections::HashMap<String, f32>>,
        cx: &mut Context<Self>,
    ) {
        let sort = config::SortSetting {
            key: view.sort.key.into(),
            direction: view.sort.dir.into(),
        };

        // Field by field, because a second hoja may own the ones this one never
        // touched. Assigning all of them unconditionally and writing the lot is
        // what let one window revert another's settings.
        self.dirty.sort |= self.state.sort != Some(sort);
        self.dirty.show_hidden |= self.state.show_hidden != Some(view.show_hidden);
        self.dirty.folders_first |= self.state.folders_first != Some(view.folders_first);

        self.state.sort = Some(sort);
        self.state.show_hidden = Some(view.show_hidden);
        self.state.folders_first = Some(view.folders_first);

        if let Some(column_widths) = column_widths {
            self.dirty.column_widths |= self.state.column_widths != column_widths;
            self.state.column_widths = column_widths;
        }

        if !self.dirty.any() {
            return;
        }

        // Throttle rather than debounce: `self.state` is already up to date, so
        // a write that is coming will carry the latest values. Rescheduling on
        // every change would let a rapid stream of them, a column drag, or a
        // watcher that keeps firing: postpone the write indefinitely.
        if self.save_task.is_some() {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(config::SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.save_task = None;
                this.state.save(this.dirty, cx);
                // Published: from here on these fields are whoever's who
                // changes them next, so a later save that only carries a window
                // resize will not drag them along with it.
                this.dirty = config::Dirty::default();
            });
        }));
    }

    /// The warning icon on a job row: everything that job could not transfer.
    fn show_failures(&mut self, job: JobId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.iter().find(|j| j.handle.id() == job) else {
            return;
        };
        // Copied rather than borrowed by index: the row underneath can be
        // dismissed while the report is open, and a still-running job keeps
        // collecting. Either would move the ground under a live index.
        let (label, failures, reasons, total) = (
            job.handle.label().to_string(),
            job.failures.clone(),
            job.reasons.clone(),
            job.errors,
        );

        let report = cx.new(|cx| FailureReport::new(label, failures, reasons, total, cx));
        cx.subscribe_in(&report, window, |this, _, _: &DismissEvent, window, cx| {
            this.close_modal(window, cx)
        })
        .detach();

        // Nothing inside takes text, so the modal holds focus itself, without
        // this escape would go to the pane behind it.
        window.focus(&report.focus_handle(cx), cx);
        self.modal = Some(Modal::Failures(report));
        cx.notify();
    }

    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        window.focus(&self.active_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// The scrim both modals sit in. `conflict_dialog` keeps its own, since it
    /// centres rather than anchoring near the top.
    /// Publish what the window is showing, when `HOJA_PROBE` asked for it.
    ///
    /// Asked for from `render`, which is the one place that runs after every
    /// change, but *deferred*: a workspace renders before the panes inside it,
    /// so reading them here reports the frame before this one. The pane footer
    /// is computed in `sync_footer` during the pane's own render and would
    /// always have trailed by one, which is worse than useless, because
    /// `sync_footer` deliberately does not notify and so nothing would have
    /// forced the frame that caught up.
    fn schedule_probe(&self, window: &mut Window) {
        if crate::probe::path().is_none() {
            return;
        }
        let panes: Vec<_> = self.panes.clone();
        let jobs: Vec<_> = self
            .jobs
            .iter()
            .map(|job| crate::probe::JobProbe {
                label: job.handle.label().to_string(),
                done: job.done.is_some(),
                errors: job.errors,
                state: job.state().name(),
                percent: job.percent(),
            })
            .collect();
        let undo_depth = self.undo_stack.len();
        // `conflict_dialog` and `open_prompt` sit outside `Modal` (see its own
        // doc comment), so a test that needs to know one is up reads it from
        // here rather than from a field that only ever answers for the three
        // the enum names.
        let modal = self
            .modal
            .as_ref()
            .map(|modal| match modal {
                Modal::Palette(_) => "palette",
                Modal::Places(_) => "places",
                Modal::Failures(_) => "failures",
            })
            .or(self.conflict_dialog.is_some().then_some("conflict"))
            .or(self.open_prompt.is_some().then_some("open-prompt"));
        let notice = self.notice.as_ref().map(|n| n.text().to_string());
        window.on_next_frame(move |_, cx| {
            // One reading for every pane, so two cannot disagree about how long
            // ago a file was written.
            let now = std::time::SystemTime::now();
            crate::probe::write(&mut crate::probe::Probe {
                panes: panes.iter().map(|pane| pane.read(cx).probe(now)).collect(),
                jobs,
                modal,
                notice,
                undo_depth,
                revision: 0, // `write` owns this
            });
        });
    }

    fn modal_scrim(&self, child: gpui::AnyElement, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .occlude()
            .absolute()
            .inset_0()
            .bg(hsla(0., 0., 0., 0.45))
            .flex()
            .justify_center()
            // Without this the modal stretches to the full height of the scrim
            // and trails empty background below its last row.
            .items_start()
            // Anchored near the top rather than centred: the list grows
            // downward, so a centred modal would shift under the cursor as
            // results narrow.
            .pt(px(80.))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_modal(window, cx)),
            )
            .child(child)
    }

    fn dismiss_jobs(&mut self, _: &DismissJobs, _window: &mut Window, cx: &mut Context<Self>) {
        let dropped: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|job| job.done.is_some())
            .map(|job| job.handle.id())
            .collect();
        self.jobs.retain(|job| job.done.is_none());
        self.pending_conflicts
            .retain(|pending| !dropped.contains(&pending.job));
        cx.notify();
    }

    /// Stop every running transfer, or start them all again.
    ///
    /// One key for the lot rather than one per row, because the row a key would
    /// have to mean is whichever you were looking at, and the strip has no
    /// selection to say which that is. The toggle on each row is how a single
    /// job is answered.
    ///
    /// The sense is taken from whether anything is still running: with one job
    /// paused and one not, this pauses the second rather than swapping them.
    fn pause_jobs(&mut self, _: &PauseJobs, _window: &mut Window, cx: &mut Context<Self>) {
        let running = self
            .jobs
            .iter()
            .any(|job| job.done.is_none() && !job.pause_requested);
        for job in &mut self.jobs {
            if job.done.is_none() {
                job.set_paused(running);
            }
        }
        cx.notify();
    }

    /// Drop conflicts queued for a job that is no longer running. Their reply
    /// channels lead to a worker that has stopped reading.
    fn purge_conflicts(&mut self, job: JobId) {
        self.pending_conflicts.retain(|pending| pending.job != job);
    }

    fn render_job_strip(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let text = colors.text;
        let muted = colors.text_muted;
        let error_color = cx.theme().status().error;
        let bar_bg = colors.element_background;
        // border_selected against element_background is two muted darks in most
        // themes, the bar was drawn correctly and simply could not be seen.
        // The bar says how much got through and nothing else, a job that
        // failed still copied whatever it copied. Failure is the badge's to
        // report, in one place rather than smeared across the row.
        let bar_fill = colors.text_accent;

        let rows: Vec<_> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(ix, job)| {
                // Captured by the click handlers below instead of `ix`.
                // `poll_jobs` drops finished clean jobs from the vector, which
                // shifts every later index down while the handlers built for
                // the previous frame still hold the old one. A click landing in
                // that window cancelled a different transfer.
                let job_id = job.handle.id();
                let progress = job.handle.progress();
                let bytes_done = progress
                    .bytes_done
                    .load(std::sync::atomic::Ordering::Relaxed);
                let bytes_total = progress
                    .bytes_total
                    .load(std::sync::atomic::Ordering::Relaxed);
                let files_total = progress
                    .files_total
                    .load(std::sync::atomic::Ordering::Relaxed);
                let walk_complete = progress
                    .walk_complete
                    .load(std::sync::atomic::Ordering::Relaxed);
                let state = job.state();

                let status = match state {
                    JobState::Cancelled => Err("cancelled".to_string()),
                    JobState::Done => Err("done".to_string()),
                    // The bar keeps moving through this one, which is the
                    // point of saying it: the file in flight has to finish
                    // before the worker can stop, and without a word here that
                    // reads as the button having done nothing.
                    JobState::Pausing => Err("pausing…".to_string()),
                    JobState::Paused => Err("paused".to_string()),
                    // The count climbs while it runs, which is the useful part:
                    // it says up front that this is 86,000 files, not 20.
                    JobState::Scanning => Err(format!("scanning… {files_total} files")),
                    JobState::Flushing => Err("flushing to device…".to_string()),
                    JobState::Conflict => Err("waiting for answer…".to_string()),
                    JobState::Running => Ok(transfer_metrics(
                        bytes_done,
                        bytes_total,
                        walk_complete,
                        job.rate,
                    )),
                };

                // Shared with the probe, so a test cannot watch a percentage
                // the bar does not draw.
                let fraction = job.fraction();

                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(26.))
                    .text_xs()
                    .text_color(text)
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(280.))
                            .truncate()
                            .child(job.handle.label().to_string()),
                    )
                    .child(
                        div().flex_1().h(px(6.)).rounded_sm().bg(bar_bg).child(
                            div()
                                .h_full()
                                .rounded_sm()
                                .bg(bar_fill)
                                .w(relative(fraction)),
                        ),
                    )
                    .child({
                        // A number grows leftwards into its own cell; the unit
                        // beside it starts at a fixed edge and so never moves.
                        let value = |width: f32, text: String| {
                            div()
                                .w(px(width))
                                .flex_none()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .overflow_hidden()
                                .child(text)
                        };
                        let unit = |width: f32, text: String| {
                            div().w(px(width)).flex_none().overflow_hidden().child(text)
                        };
                        let row = div()
                            .flex_none()
                            .w(px(STATUS_W))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            // Monospace, so a digit and a decimal point and a
                            // space all take the same width and nothing in the
                            // block reflows as the numbers climb. `tnum` stays
                            // for the machine with no mono font installed: it
                            // evens out the digits of a proportional face, which
                            // is most of the same benefit.
                            .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                                "tnum".to_string(),
                                1,
                            )])))
                            .when_some(crate::theming::numeric_font(cx), |el, family| {
                                el.font_family(family)
                            })
                            .text_color(muted);
                        match status {
                            // One sentence, right-aligned across the whole block
                            // so it ends where the columns end.
                            Err(sentence) => row.child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .overflow_hidden()
                                    .child(sentence),
                            ),
                            Ok(m) => row
                                .child(
                                    // "450 MB / 687 MB" is one reading, not two
                                    // numbers that happen to be adjacent. Set at
                                    // the row's spacing the slash floated in the
                                    // middle of it, tied to neither side; at half
                                    // that the phrase closes up and the wider gap
                                    // to the rate does the separating.
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(TIGHT_GAP))
                                        .child(value(VALUE_W, m.done.value))
                                        .child(unit(SIZE_UNIT_W, m.done.unit))
                                        .child(
                                            div()
                                                .w(px(SEP_W))
                                                .flex_none()
                                                .flex()
                                                .flex_row()
                                                .justify_center()
                                                .child("/"),
                                        )
                                        .child(unit(TOTAL_W, m.total.joined())),
                                )
                                .child(value(RATE_VALUE_W, m.rate.value))
                                .child(unit(RATE_UNIT_W, m.rate.unit))
                                .child(value(LEFT_VALUE_W, m.left.value))
                                .child(unit(LEFT_UNIT_W, m.left.unit)),
                        }
                    })
                    // Only once something has failed, so the row is otherwise
                    // exactly as wide as it was.
                    .when(job.errors > 0, |el| {
                        el.child(
                            // Fixed width with the badge held to its right edge:
                            // the count climbs while the job runs, and growing
                            // leftwards into reserved space keeps it from
                            // shoving the progress bar about. Sized for a count
                            // in the thousands, which a tree of symlinks onto a
                            // filesystem that cannot hold them will reach.
                            div()
                                .flex_none()
                                .w(px(BADGE_W))
                                .flex()
                                .flex_row()
                                .justify_end()
                                .child(
                                    div()
                                        .id(("job-errors", ix))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(gpui::Hsla {
                                            a: 0.15,
                                            ..error_color
                                        })
                                        .text_color(error_color)
                                        .cursor_pointer()
                                        .hover(|s| {
                                            s.bg(gpui::Hsla {
                                                a: 0.28,
                                                ..error_color
                                            })
                                        })
                                        .child(Icon::from_path(
                                            "icons/file_icons/warning.svg",
                                            error_color,
                                        ))
                                        .child(if job.errors == 1 {
                                            "1 failed".to_string()
                                        } else {
                                            format!(
                                                "{} failed",
                                                notifications::count(job.errors as u64)
                                            )
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.show_failures(job_id, window, cx)
                                        })),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(PAUSE_W))
                            .flex()
                            .flex_row()
                            .justify_center()
                            .when(state.live(), |el| {
                                // The icon says what pressing it will do, not
                                // what the job is doing: a running job offers
                                // "pause", a stopped one offers "play". The
                                // word beside it in the status block is what
                                // reports the state.
                                let resume = state.parked_or_parking();
                                el.child(
                                    div()
                                        .id(("job-pause", ix))
                                        .px_1()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(colors.element_hover))
                                        .child(Icon::from_path(
                                            if resume {
                                                "icons/file_icons/play.svg"
                                            } else {
                                                "icons/file_icons/pause.svg"
                                            },
                                            muted,
                                        ))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let Some(at) = this
                                                .jobs
                                                .iter()
                                                .position(|j| j.handle.id() == job_id)
                                            else {
                                                return;
                                            };
                                            if this.jobs[at].done.is_some() {
                                                return;
                                            }
                                            let asked = this.jobs[at].pause_requested;
                                            this.jobs[at].set_paused(!asked);
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(("job-x", ix))
                            .flex_none()
                            .px_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(colors.element_hover))
                            .child("✕")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(at) =
                                    this.jobs.iter().position(|j| j.handle.id() == job_id)
                                else {
                                    return;
                                };
                                if this.jobs[at].done.is_some() {
                                    this.jobs.remove(at);
                                } else {
                                    this.jobs[at].handle.cancel();
                                }
                                // Either way the worker stops answering.
                                this.purge_conflicts(job_id);
                                cx.notify();
                            })),
                    )
            })
            .collect();

        div()
            .flex_none()
            .flex()
            .flex_col()
            .bg(colors.title_bar_background)
            .border_t_1()
            .border_color(colors.border)
            .when_some(self.notice.as_ref(), |el, notice| {
                let color = if notice.is_problem() {
                    error_color
                } else {
                    muted
                };
                el.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(26.))
                        .text_xs()
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_color(color)
                                .child(notice.text().to_string()),
                        )
                        .child(
                            div()
                                .id("notice-x")
                                .flex_none()
                                .px_1()
                                .text_color(muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(colors.element_hover))
                                .child("dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notice = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .children(rows)
    }

    // ---- split/focus handlers --------------------------------------------

    fn split_left(&mut self, _: &SplitLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitDirection::Left, window, cx);
    }
    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitDirection::Right, window, cx);
    }
    fn split_up(&mut self, _: &SplitUp, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitDirection::Up, window, cx);
    }
    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitDirection::Down, window, cx);
    }

    fn focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_in_direction(SplitDirection::Left, window, cx);
    }
    fn focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_in_direction(SplitDirection::Right, window, cx);
    }
    fn focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_in_direction(SplitDirection::Up, window, cx);
    }
    fn focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_in_direction(SplitDirection::Down, window, cx);
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(1, window, cx);
    }

    fn focus_previous(&mut self, _: &FocusPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(-1, window, cx);
    }

    /// Tab: step to the next pane, wrapping.
    ///
    /// Order is the order panes were created, not their arrangement on screen.
    /// For the two-pane case they are the same, and beyond it a cycle only has
    /// to be predictable and reversible, which this is: `activate_in_direction`
    /// is what answers "the one to the left of this".
    fn cycle_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() < 2 {
            return;
        }
        let current = self
            .panes
            .iter()
            .position(|pane| pane == &self.active_pane)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(self.panes.len() as isize) as usize;
        let pane = self.panes[next].clone();
        // Focusing is enough: a pane emits Focus and the workspace makes it
        // active in response.
        window.focus(&pane.focus_handle(cx), cx);
    }

    fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane.clone();
        self.remove_pane(&pane, window, cx);
    }
}

fn parent_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = paths
        .iter()
        .filter_map(|p| p.parent().map(std::path::Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn delete_failure_notice(failures: &[(PathBuf, std::io::Error)]) -> Option<Notice> {
    let (path, err) = failures.first()?;
    Some(Notice::Problem(match failures.len() {
        1 => format!("Could not delete {}: {err}", file_label(path)),
        n => format!("Could not delete {n} items. {}: {err}", file_label(path)),
    }))
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window had no title at all, which leaves an empty entry in the
        // task switcher. Name it for the active pane's directory.
        let dir = self.active_pane.read(cx).location().key();
        let title = file_label(&dir);
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

        self.schedule_probe(window);

        // Bindings are scoped to the focused pane's "DirPane" context, but the actions
        // bubble from the focused pane up to here, so the handlers live on the
        // workspace where the active pane is known.
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::split_left))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_up))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::dismiss_jobs))
            .on_action(cx.listener(Self::pause_jobs))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(|this, action, _, cx| this.delete(action, cx)))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::toggle_places))
            .child(self.center.render(&self.active_pane, window, cx))
            // Search progress moved into the pane footer, where it belongs to
            // the pane doing the searching, a background pane's used to be
            // invisible. The strip is now transfers and notices only.
            .when(!self.jobs.is_empty() || self.notice.is_some(), |el| {
                el.child(self.render_job_strip(cx))
            })
            .when_some(self.modal.as_ref().map(Modal::element), |el, modal| {
                el.child(self.modal_scrim(modal, cx))
            })
            .when_some(self.conflict_dialog.clone(), |el, dialog| {
                el.child(
                    div()
                        .occlude()
                        .absolute()
                        .inset_0()
                        .bg(hsla(0., 0., 0., 0.45))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(dialog),
                )
            })
            .when_some(self.open_prompt.clone(), |el, dialog| {
                el.child(
                    div()
                        .occlude()
                        .absolute()
                        .inset_0()
                        .bg(hsla(0., 0., 0., 0.45))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(dialog),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> ClipboardSet {
        ClipboardSet {
            paths: paths.iter().map(PathBuf::from).collect(),
            cut: false,
        }
    }

    #[test]
    fn external_wins_only_when_it_has_moved_on_from_the_baseline() {
        let a = Some(set(&["/a"]));
        let b = Some(set(&["/b"]));

        // Unchanged since the archive copy: nothing else has claimed the
        // clipboard, so the archive selection is still what was asked for.
        assert!(!superseded(&a, &a));
        // A real copy happened somewhere else afterward.
        assert!(superseded(&b, &a));
        // The clipboard was empty at copy time and now holds something: the
        // same case, an empty clipboard is a baseline too.
        assert!(superseded(&a, &None));
        // Nothing external now, whatever the baseline was.
        assert!(!superseded(&None, &a));
        assert!(!superseded(&None, &None));
    }

    #[test]
    fn a_figure_keeps_its_unit_separate() {
        // The layout gives the number and the unit cells of their own, so they
        // have to arrive apart. Split at the last space, since a unit can hold
        // one: "28s left".
        assert_eq!(
            Figure::split("1.5 MB"),
            Figure {
                value: "1.5".into(),
                unit: "MB".into()
            }
        );
        assert_eq!(
            Figure::split("28s left"),
            Figure {
                value: "28s".into(),
                unit: "left".into()
            }
        );
        assert_eq!(
            Figure::split("1h 20m left"),
            Figure {
                value: "1h 20m".into(),
                unit: "left".into()
            }
        );
        // Nothing to split: the whole of it is the unit, so it lands in the
        // cell that does not move.
        assert_eq!(
            Figure::split("…"),
            Figure {
                value: String::new(),
                unit: "…".into()
            }
        );
    }

    #[test]
    fn metrics_say_only_what_they_know() {
        let fig = |v: &str, u: &str| Figure {
            value: v.into(),
            unit: u.into(),
        };

        // Mid-scan: no denominator yet, so no time remaining either.
        assert_eq!(
            transfer_metrics(1_200_000, 0, false, None),
            Metrics {
                done: fig("1.1", "MB"),
                total: fig("", "…"),
                ..Default::default()
            }
        );
        assert_eq!(
            transfer_metrics(1_200_000, 0, false, Some(50_000_000.)),
            Metrics {
                done: fig("1.1", "MB"),
                total: fig("", "…"),
                rate: fig("48", "MB/s"),
                left: Figure::default(),
            }
        );
        // Scan finished: everything.
        assert_eq!(
            transfer_metrics(100_000_000, 500_000_000, true, Some(50_000_000.)),
            Metrics {
                done: fig("95.4", "MB"),
                total: fig("476.8", "MB"),
                rate: fig("48", "MB/s"),
                left: fig("08s", "left"),
            }
        );
        // A stalled transfer must not promise an infinite wait.
        assert_eq!(
            transfer_metrics(100, 500_000_000, true, Some(0.)),
            Metrics {
                done: fig("100", "B"),
                total: fig("476.8", "MB"),
                ..Default::default()
            }
        );
        // Nothing left to do is not "0s left" forever.
        assert_eq!(
            transfer_metrics(500, 500, true, Some(1000.)),
            Metrics {
                done: fig("500", "B"),
                total: fig("500", "B"),
                rate: fig("1000", "B/s"),
                left: Figure::default(),
            }
        );
    }

    #[test]
    fn every_metric_fits_the_column_it_is_given() {
        // The cells are fixed, so the widest each can hold has to fit. At
        // text_xs with tabular figures a digit is about 6.5px; this pins the
        // assumption so a formatting change that outgrows a column is caught
        // here rather than by truncation on screen.
        let wide = transfer_metrics(
            1023 * 1024 * 1024,
            1023 * 1024 * 1024 + 1,
            true,
            Some(1023. * 1024. * 1024.),
        );
        let fits = |text: &str, width: f32| {
            assert!(
                text.chars().count() as f32 * CHAR_W <= width,
                "{text:?} needs more than {width}px"
            );
        };
        fits(&wide.done.value, VALUE_W);
        fits(&wide.done.unit, SIZE_UNIT_W);
        fits(&wide.rate.value, RATE_VALUE_W);
        fits(&wide.rate.unit, RATE_UNIT_W);
        // The longest forms of the remaining time, which decide its two cells.
        // Every one `format_remaining` can return, not a shape it used to: the
        // old check pinned "1h 59m", which is un-padded and one character short
        // of what the function had already started printing, and the cell it
        // guards clips from the leading edge rather than the trailing one, so an
        // overflow there is a wrong number rather than a visible ellipsis.
        for longest in ["59m 59s", "23h 59m", "99d 23h", "99d+"] {
            fits(longest, LEFT_VALUE_W);
        }
        fits("left", LEFT_UNIT_W);
    }

    #[test]
    fn the_status_block_is_exactly_as_wide_as_what_it_holds() {
        // The sentence states ("scanning…", "done") right-align across
        // STATUS_W, so it has to end where the columns end. Derived rather than
        // guessed, but derived arithmetic still drifts when a cell is added,
        // and drifting the other way steals width from the progress bar.
        assert_eq!(
            SIZE_W,
            VALUE_W + TIGHT_GAP + SIZE_UNIT_W + TIGHT_GAP + SEP_W + TIGHT_GAP + TOTAL_W,
            "the four cells of the size phrase and the three gaps between them"
        );
        // The pause toggle's cell is reserved on a finished row too, so that
        // the ✕ does not slide leftwards at the moment someone is reaching for
        // it. That only holds while the reservation is the size of what it
        // reserves for: a 16px icon inside px_1, which is 4px a side.
        assert_eq!(
            PAUSE_W,
            16. + 4. * 2.,
            "the pause cell no longer matches the control it holds"
        );
        // The total is one cell now, so it has to hold the number and the unit
        // together. `format_size` always prints one decimal and promotes past
        // 1023, except below a kilobyte where there is no tenth of a byte, so
        // these are the widest shapes it has.
        for widest in ["1023.9 PB", "99.9 GB", "1023 B"] {
            assert!(
                widest.chars().count() as f32 * CHAR_W <= TOTAL_W,
                "{widest:?} outgrew the total's cell"
            );
        }
        assert_eq!(
            STATUS_W,
            SIZE_W
                + GAP
                + RATE_VALUE_W
                + GAP
                + RATE_UNIT_W
                + GAP
                + LEFT_VALUE_W
                + GAP
                + LEFT_UNIT_W,
            "the size phrase, the rate, the remaining time, and the gaps between"
        );
    }

    #[test]
    fn the_rate_estimate_smooths_rather_than_jumps() {
        // First sample is taken as-is; there is nothing to average with.
        let first = fold_rate(None, 1000, 1.0).unwrap();
        assert!((first - 1000.).abs() < 1e-6);

        // A tenfold burst must not drag the estimate far in one step: at this
        // weight it moves about half again, not ten times.
        let after = fold_rate(Some(1000.), 10_000, 1.0).unwrap();
        assert!(after > 1000. && after < 2000., "got {after}");

        // Sustained, it converges on the new speed. The count is generous
        // because the weight sets how long that takes: at 0.06 the estimate is
        // within a percent after about 75 samples, which at a 120ms poll is the
        // couple of seconds the constant is documented to average over.
        let mut rate = Some(1000.);
        for _ in 0..150 {
            rate = fold_rate(rate, 10_000, 1.0);
        }
        assert!((rate.unwrap() - 10_000.).abs() < 100., "got {rate:?}");

        // An interval too short to divide by leaves the estimate alone.
        assert_eq!(fold_rate(Some(1234.), 5, 0.001), Some(1234.));
    }
}
