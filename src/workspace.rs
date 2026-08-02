use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    App, Context, DismissEvent, Entity, EntityId, FocusHandle, Focusable, Subscription, Task,
    Window, actions, div, hsla, prelude::*, px, relative,
};
use hoja_transfer::{
    ConflictDecision, Event as JobEvent, JobHandle, JobId, JobPolicy, JobSpec, JobSummary,
    Operation, Outcome, Phase, TrashedItem,
};
use theme::ActiveTheme;

use crate::clipboard::{self, ClipboardSet};
use crate::command_palette::{self, CommandPalette};
use crate::place_finder::{self, PlaceEvent, PlaceFinder};
use crate::conflict_dialog::ConflictDialog;
use crate::dir_pane::{DirPane, PaneEvent};
use crate::config::{self, Settings, State};
use crate::notifications;
use crate::fs::ViewSettings;
use crate::fs;
use crate::pane_group::{PaneGroup, SplitDirection};

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
}

impl Modal {
    fn element(&self) -> gpui::AnyElement {
        match self {
            Modal::Palette(palette) => palette.clone().into_any_element(),
            Modal::Places(finder) => finder.clone().into_any_element(),
        }
    }
}

struct PendingConflict {
    job: JobId,
    dest: PathBuf,
    reply: std::sync::mpsc::Sender<ConflictDecision>,
}

/// A running or finished transfer job as the UI tracks it.
struct JobView {
    handle: JobHandle,
    /// Where the job writes, for refreshing affected panes on completion.
    dest_dir: PathBuf,
    /// Source parent dirs, refreshed after moves.
    src_parents: Vec<PathBuf>,
    done: Option<Outcome>,
    errors: usize,
    last_error: Option<String>,
    /// When the transfer started, so a job short enough to have been watched
    /// does not raise a notification about it — see `notifications::NOTIFY_AFTER`.
    started: std::time::Instant,
    /// Smoothed bytes per second, and the sample it was folded from.
    rate: Option<f64>,
    last_sample: (std::time::Instant, u64),
}

/// Weight of the newest sample in the rate estimate.
///
/// A 120ms window over a tree of small files is violently bursty — one large
/// file lands and the instantaneous figure jumps by an order of magnitude — and
/// a number that jitters like that is worse than no number.
///
/// The weight sets a time constant of roughly `JOB_POLL_INTERVAL / w`, so this
/// averages over about two seconds. It was 0.25 to begin with, which is half a
/// second, and at that the reading chased every burst: 1.6, 2.1, 3.1, 1.9 GB/s
/// in consecutive frames of the same steady copy. Two seconds still follows a
/// real change of speed — crossing onto slower media, or into a directory of
/// small files — within a few frames, which is as fast as anyone can read it.
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
            // No space to split on — "…" while the scan is still running.
            None => Figure {
                value: String::new(),
                unit: formatted.to_string(),
            },
        }
    }
}

/// Column widths, in the order they are laid out. Fixed, and sized for the
/// longest each can hold: four digits and a decimal point, "MB/s", "1h 59m",
/// "left". A cell sized to its content instead makes the progress bar beside it
/// grow and shrink on every repaint, since the bar is what absorbs the
/// difference — and the bar is the one thing on the strip that should hold
/// still.
const VALUE_W: f32 = 34.;
const SIZE_UNIT_W: f32 = 20.;
const SEP_W: f32 = 10.;
const RATE_VALUE_W: f32 = 32.;
const RATE_UNIT_W: f32 = 32.;
const LEFT_VALUE_W: f32 = 44.;
const LEFT_UNIT_W: f32 = 28.;
/// Everything above plus the gaps, so the states that show a sentence line up
/// with the states that show numbers.
const STATUS_W: f32 = VALUE_W * 2.
    + SIZE_UNIT_W * 2.
    + SEP_W
    + RATE_VALUE_W
    + RATE_UNIT_W
    + LEFT_VALUE_W
    + LEFT_UNIT_W
    + 30.;

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
    // and dividing by it would promise infinity — say nothing instead.
    if let Some(rate) = rate.filter(|r| *r > 1.) {
        metrics.rate = Figure::split(&format!("{}/s", fs::format_size(rate as u64)));
        if walk_complete && total > done {
            let left = (total - done) as f64 / rate;
            metrics.left = Figure::split(&format!("{} left", fs::format_remaining(left)));
        }
    }
    metrics
}

impl JobView {
    /// Fold the current byte count into the rate estimate.
    fn sample(&mut self, now: std::time::Instant, bytes: u64) {
        let (then, before) = self.last_sample;
        let secs = now.duration_since(then).as_secs_f64();
        let folded = fold_rate(self.rate, bytes.saturating_sub(before), secs);
        if secs >= 0.05 {
            self.last_sample = (now, bytes);
        }
        self.rate = folded;
    }
}

/// Owns the pane tree and the notion of which pane is active.
///
/// Following Zed: the tree is *not* the source of truth for activation. A separate
/// `active_pane` handle plus a flat `panes` list track that, and focus drives it — a
/// pane emits `PaneEvent::Focus` on focus-in and we make it active in response. Clicking
/// a pane focuses it, so activation needs no separate click plumbing.
pub struct Workspace {
    center: PaneGroup,
    panes: Vec<Entity<DirPane>>,
    active_pane: Entity<DirPane>,
    focus_handle: FocusHandle,
    pane_subscriptions: HashMap<EntityId, Subscription>,
    /// Internal file clipboard — the source of truth for in-app paste. The
    /// system clipboard is mirrored on write and consulted on read only to
    /// interoperate with other applications.
    clipboard: Option<ClipboardSet>,
    jobs: Vec<JobView>,
    /// Deletions, newest last. Each entry is one `Delete` press, so undo
    /// restores a multi-selection in one go.
    undo_stack: Vec<Vec<TrashedItem>>,
    notice: Option<Notice>,
    /// One slot, so opening either closes the other. Two fields let ctrl-p on
    /// top of the palette leave both open and both rendered.
    modal: Option<Modal>,
    /// Last title pushed to the compositor, so an unchanged one is not re-sent
    /// on every frame.
    title: String,
    /// What hoja remembers between runs. Written from `save_state`, never read
    /// after startup — the panes own the live values.
    state: State,
    save_task: Option<Task<()>>,
    /// Which remembered fields are this instance's to publish — see `Dirty`.
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
            let mut pane = DirPane::new(start_dir, view, window, cx);
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
                    this.remember_view(cx);
                }),
                // The throttled write is a `Task` this entity owns, so closing
                // the window drops it — cancelling the timer before it ever
                // fires. Anything toggled inside the last SAVE_DEBOUNCE window
                // went with it. Write inline here instead; there is no executor
                // left to defer to.
                //
                // Release, not `on_app_quit`: closing the last window drops the
                // window, which releases this entity, and only *then* quits the
                // app. A quit observer therefore runs when the workspace is
                // already gone, and its callback — which needs `&mut self` —
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
            PaneEvent::ViewChanged => this.remember_view(cx),
            PaneEvent::Notice(message) => {
                this.set_notice(Some(Notice::Problem(message.clone())), cx)
            }
            PaneEvent::Transfer { op, sources, dest } => {
                this.spawn_transfer(*op, sources.clone(), dest.clone(), window, cx)
            }
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
        dir: PathBuf,
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
        let dir = self.active_pane.read(cx).dir().to_path_buf();
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

        if was_active
            && let Some(fallback) = self.panes.last().cloned()
        {
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
        // nothing before the first paint — a chord pressed at startup does nothing.
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
        let paths = self.active_pane.read(cx).selected_paths();
        if paths.is_empty() {
            return;
        }
        let set = ClipboardSet { paths, cut };
        clipboard::mirror_to_system(&set);
        self.clipboard = Some(set);
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        self.stash_selection(false, cx);
    }

    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        self.stash_selection(true, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let dest_dir = self.active_pane.read(cx).dir().to_path_buf();
        let internal = self.clipboard.clone();

        // The external read shells out to wl-paste, which blocks on the current
        // clipboard owner — so it runs on the background executor. External
        // content wins when another app owns the selection; internal otherwise.
        let task = cx.spawn_in(window, async move |this, cx| {
            let external = cx
                .background_spawn(async move { clipboard::read_external() })
                .await;

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
                    done: None,
                    errors: 0,
                    last_error: None,
                    started: std::time::Instant::now(),
                    rate: None,
                    last_sample: (std::time::Instant::now(), 0),
                });
                self.ensure_polling(window, cx);
                cx.notify();
            }
            // Drag-and-drop greys an illegal target, but paste has no such
            // pre-check, so this is the only way the refusal is ever seen.
            Err(err) => self.set_notice(Some(Notice::Problem(err.to_string())), cx),
        }
    }

    /// One poll loop for all jobs, alive only while jobs exist. 120ms matches
    /// the "sample progress on a timer" design — frame-driven polling would
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

        let now = std::time::Instant::now();
        for job in &mut self.jobs {
            let job_id = job.handle.id();
            if job.done.is_none() {
                let bytes = job
                    .handle
                    .progress()
                    .bytes_done
                    .load(std::sync::atomic::Ordering::Relaxed);
                job.sample(now, bytes);
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
                        job.last_error = Some(format!("{}: {error}", path.display()));
                    }
                    JobEvent::Warning { .. } => {}
                    JobEvent::Done(summary) => {
                        job.done = Some(summary.outcome);
                        finished_jobs.push(job_id);
                        job.errors = summary.errors.len();
                        if let Some((path, error)) = summary.errors.first() {
                            job.last_error = Some(format!("{}: {error}", path.display()));
                        }
                        finished_dirs.push(job.dest_dir.clone());
                        finished_dirs.extend(job.src_parents.iter().cloned());
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
    /// Not for a job you watched happen, and not for one you cancelled — you
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
                "{files} of {} to {where_to} — {} failed",
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
        cx.subscribe_in(
            &dialog,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                this.conflict_dialog = None;
                window.focus(&this.active_pane.focus_handle(cx), cx);
                this.maybe_show_conflict(window, cx);
                cx.notify();
            },
        )
        .detach();

        window.focus(&dialog.focus_handle(cx), cx);
        self.conflict_dialog = Some(dialog);
        cx.notify();
    }

    fn new_folder(&mut self, _: &NewFolder, _window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active_pane.read(cx).dir().to_path_buf();
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
    /// which is what makes `Undo` possible — an unlinked file cannot come back.
    ///
    /// There is deliberately no confirmation dialog: undo is the safety net, and
    /// a dialog that is always dismissed protects nobody.
    fn delete(&mut self, _: &Delete, cx: &mut Context<Self>) {
        let paths = self.active_pane.read(cx).selected_paths();
        if paths.is_empty() {
            return;
        }
        let parents = parent_dirs(&paths);
        let pane = self.active_pane.clone();

        // Each item is a rename, so this is fast — but "fast" is a property of
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
                    this.undo_stack.push(trashed);
                    if this.undo_stack.len() > UNDO_DEPTH {
                        this.undo_stack.remove(0);
                    }
                }
                this.set_notice(delete_failure_notice(&failures), cx);
                this.refresh_dirs(&parents, cx);
            });
        })
        .detach();
    }

    /// Put the most recent deletion back.
    fn undo(&mut self, _: &Undo, cx: &mut Context<Self>) {
        let Some(batch) = self.undo_stack.pop() else {
            self.set_notice(Some(Notice::Info("Nothing to undo".to_string())), cx);
            return;
        };
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
                    this.undo_stack
                        .push(failures.into_iter().map(|(item, _)| item).collect());
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
            let pane_dir = pane.read(cx).dir().to_path_buf();
            if dirs.contains(&pane_dir) {
                pane.update(cx, |pane, cx| pane.refresh(cx));
            }
        }
        cx.notify();
    }

    /// Put the selection back on restored items, in whichever pane shows them.
    fn select_in_panes(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for pane in &self.panes {
            let pane_dir = pane.read(cx).dir().to_path_buf();
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

        // Any *other* modal — the place finder — still holds focus here, and
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
        cx.subscribe_in(&finder, window, |this, _, event: &PlaceEvent, window, cx| {
            match event {
                PlaceEvent::Open(path) => {
                    this.active_pane
                        .update(cx, |pane, cx| pane.navigate_to(path.clone(), cx));
                    window.focus(&this.active_pane.focus_handle(cx), cx);
                }
                PlaceEvent::Mount { device, label } => {
                    this.mount_and_open(device.clone(), label.clone(), cx)
                }
            }
        })
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
                        .update(cx, |pane, cx| pane.navigate_to(path, cx));
                }
                Err(err) => this.set_notice(
                    Some(Notice::Problem(format!("Could not mount {label}: {err}"))),
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
                cx.background_executor()
                    .timer(config::SAVE_DEBOUNCE)
                    .await;
                if !config::drain_changes(&rx) {
                    continue;
                }
                if this
                    .update(cx, |this, cx| this.apply_settings(config::Settings::load(), cx))
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
        // it has one. Where it has none — a file that only sets a theme — what
        // was remembered still stands, which is why the real state goes in here
        // and not an empty one. Passing `State::default()` reset every pane to
        // the compiled defaults on any settings edit, and then wrote the reset
        // out through `remember_view` below.
        let view = config::initial_view(&settings, &self.state, config::Winner::Settings);
        for pane in &self.panes {
            pane.update(cx, |pane, cx| pane.set_view_settings(view, cx));
        }
        self.remember_view(cx);
        cx.notify();
    }

    /// Record what the active pane is showing, and write it out shortly.
    ///
    /// Debounced because a column drag changes this on every frame; without it
    /// a single resize would be a few hundred writes.
    pub fn remember_view(&mut self, cx: &mut Context<Self>) {
        let pane = self.active_pane.read(cx);
        let view = pane.view_settings();
        let sort = config::SortSetting {
            key: view.sort.key.into(),
            direction: view.sort.dir.into(),
        };
        let column_widths = pane.column_widths();

        // Field by field, because a second hoja may own the ones this one never
        // touched. Assigning all of them unconditionally and writing the lot is
        // what let one window revert another's settings.
        self.dirty.sort |= self.state.sort != Some(sort);
        self.dirty.show_hidden |= self.state.show_hidden != Some(view.show_hidden);
        self.dirty.folders_first |= self.state.folders_first != Some(view.folders_first);
        self.dirty.column_widths |= self.state.column_widths != column_widths;

        self.state.sort = Some(sort);
        self.state.show_hidden = Some(view.show_hidden);
        self.state.folders_first = Some(view.folders_first);
        self.state.column_widths = column_widths;

        if !self.dirty.any() {
            return;
        }

        // Throttle rather than debounce: `self.state` is already up to date, so
        // a write that is coming will carry the latest values. Rescheduling on
        // every change would let a rapid stream of them — a column drag, or a
        // watcher that keeps firing — postpone the write indefinitely.
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

    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        window.focus(&self.active_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// The scrim both modals sit in. `conflict_dialog` keeps its own, since it
    /// centres rather than anchoring near the top.
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
        // themes — the bar was drawn correctly and simply could not be seen.
        let bar_fill = colors.text_accent;
        let bar_done = cx.theme().status().success;

        let rows: Vec<_> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(ix, job)| {
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
                let phase =
                    Phase::from_u8(progress.phase.load(std::sync::atomic::Ordering::Relaxed));

                let status = match (job.done, phase) {
                    (Some(Outcome::Cancelled), _) => Err("cancelled".to_string()),
                    (Some(_), _) if job.errors > 0 => {
                        Err(job.last_error.clone().unwrap_or_else(|| "errors".into()))
                    }
                    (Some(_), _) => Err("done".to_string()),
                    // The count climbs while it runs, which is the useful part:
                    // it says up front that this is 86,000 files, not 20.
                    (None, Phase::Scanning) => Err(format!("scanning… {files_total} files")),
                    (None, Phase::Flushing) => Err("flushing to device…".to_string()),
                    (None, Phase::AwaitingConflict) => Err("waiting for answer…".to_string()),
                    _ => Ok(transfer_metrics(
                        bytes_done,
                        bytes_total,
                        walk_complete,
                        job.rate,
                    )),
                };

                let is_done = job.done.is_some();
                let fraction = if walk_complete && bytes_total > 0 {
                    (bytes_done as f32 / bytes_total as f32).clamp(0., 1.)
                } else if is_done {
                    1.
                } else {
                    0. // indeterminate until the walk finishes; no backwards bars
                };

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
                        div()
                            .flex_1()
                            .h(px(6.))
                            .rounded_sm()
                            .bg(bar_bg)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_sm()
                                    .bg(if job.errors > 0 {
                                        error_color
                                    } else if is_done {
                                        bar_done
                                    } else {
                                        bar_fill
                                    })
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
                            div()
                                .w(px(width))
                                .flex_none()
                                .overflow_hidden()
                                .child(text)
                        };
                        let row = div()
                            .flex_none()
                            .w(px(STATUS_W))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            // Tabular figures: Noto Sans carries `tnum`, so every
                            // digit takes the same width and a number changing
                            // does not reflow the cell it sits in. Without it a
                            // "1" is markedly narrower than an "8" and the text
                            // shuffles on every repaint.
                            .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                                "tnum".to_string(),
                                1,
                            )])))
                            .text_color(if job.errors > 0 { error_color } else { muted });
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
                                .child(value(VALUE_W, m.total.value))
                                .child(unit(SIZE_UNIT_W, m.total.unit))
                                .child(value(RATE_VALUE_W, m.rate.value))
                                .child(unit(RATE_UNIT_W, m.rate.unit))
                                .child(value(LEFT_VALUE_W, m.left.value))
                                .child(unit(LEFT_UNIT_W, m.left.unit)),
                        }
                    })
                    .child(
                        div()
                            .id(("job-x", ix))
                            .flex_none()
                            .px_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(colors.element_hover))
                            .child(if is_done { "dismiss" } else { "✕" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(job) = this.jobs.get(ix) else { return };
                                let id = job.handle.id();
                                if job.done.is_some() {
                                    this.jobs.remove(ix);
                                } else {
                                    job.handle.cancel();
                                }
                                // Either way the worker stops answering.
                                this.purge_conflicts(id);
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
                let color = if notice.is_problem() { error_color } else { muted };
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
    /// to be predictable and reversible, which this is — `activate_in_direction`
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
        let dir = self.active_pane.read(cx).dir().to_path_buf();
        let title = file_label(&dir);
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

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
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(|this, action, _, cx| this.delete(action, cx)))
            .on_action(cx.listener(|this, action, _, cx| this.undo(action, cx)))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::toggle_places))
            .child(self.center.render(&self.active_pane, window, cx))
            // Search progress moved into the pane footer, where it belongs to
            // the pane doing the searching — a background pane's used to be
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
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_figure_keeps_its_unit_separate() {
        // The layout gives the number and the unit cells of their own, so they
        // have to arrive apart. Split at the last space, since a unit can hold
        // one — "28s left".
        assert_eq!(
            Figure::split("1.5 MB"),
            Figure { value: "1.5".into(), unit: "MB".into() }
        );
        assert_eq!(
            Figure::split("28s left"),
            Figure { value: "28s".into(), unit: "left".into() }
        );
        assert_eq!(
            Figure::split("1h 20m left"),
            Figure { value: "1h 20m".into(), unit: "left".into() }
        );
        // Nothing to split: the whole of it is the unit, so it lands in the
        // cell that does not move.
        assert_eq!(
            Figure::split("…"),
            Figure { value: String::new(), unit: "…".into() }
        );
    }

    #[test]
    fn metrics_say_only_what_they_know() {
        let fig = |v: &str, u: &str| Figure { value: v.into(), unit: u.into() };

        // Mid-scan: no denominator yet, so no time remaining either.
        assert_eq!(
            transfer_metrics(1_200_000, 0, false, None),
            Metrics { done: fig("1.1", "MB"), total: fig("", "…"), ..Default::default() }
        );
        assert_eq!(
            transfer_metrics(1_200_000, 0, false, Some(50_000_000.)),
            Metrics {
                done: fig("1.1", "MB"),
                total: fig("", "…"),
                rate: fig("47.7", "MB/s"),
                left: Figure::default(),
            }
        );
        // Scan finished: everything.
        assert_eq!(
            transfer_metrics(100_000_000, 500_000_000, true, Some(50_000_000.)),
            Metrics {
                done: fig("95.4", "MB"),
                total: fig("477", "MB"),
                rate: fig("47.7", "MB/s"),
                left: fig("8s", "left"),
            }
        );
        // A stalled transfer must not promise an infinite wait.
        assert_eq!(
            transfer_metrics(100, 500_000_000, true, Some(0.)),
            Metrics { done: fig("100", "B"), total: fig("477", "MB"), ..Default::default() }
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
                text.chars().count() as f32 * 6.5 <= width,
                "{text:?} needs more than {width}px"
            );
        };
        fits(&wide.done.value, VALUE_W);
        fits(&wide.done.unit, SIZE_UNIT_W);
        fits(&wide.rate.value, RATE_VALUE_W);
        fits(&wide.rate.unit, RATE_UNIT_W);
        // The longest forms of the remaining time, which decide its two cells.
        fits("1h 59m", LEFT_VALUE_W);
        fits("left", LEFT_UNIT_W);
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
