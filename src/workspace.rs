use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, Context, DismissEvent, Entity, EntityId, FocusHandle, Focusable, Subscription, Task,
    Window, actions, div, hsla, prelude::*, px, relative,
};
use pane_transfer::{
    ConflictDecision, Event as JobEvent, JobHandle, JobId, JobPolicy, JobSpec, Operation, Outcome,
    Phase, TrashedItem,
};
use theme::ActiveTheme;

use crate::clipboard::{self, ClipboardSet};
use crate::command_palette::{self, CommandPalette};
use crate::conflict_dialog::ConflictDialog;
use crate::dir_pane::{DirPane, PaneEvent};
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
    palette: Option<Entity<CommandPalette>>,
    poll_task: Option<Task<()>>,
    /// Conflicts wait here while one dialog is up; one worker blocks per job,
    /// so concurrent jobs can queue several. Tagged with the job so cancelling
    /// can drop the ones whose worker is gone.
    pending_conflicts: VecDeque<PendingConflict>,
    conflict_dialog: Option<Entity<ConflictDialog>>,
}

impl Workspace {
    pub fn new(start_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| DirPane::new(start_dir, ViewSettings::default(), window, cx));
        let subscription = Self::subscribe_to_pane(&pane, window, cx);

        Self {
            center: PaneGroup::new(pane.clone()),
            panes: vec![pane.clone()],
            active_pane: pane.clone(),
            focus_handle: cx.focus_handle(),
            pane_subscriptions: HashMap::from([(pane.entity_id(), subscription)]),
            clipboard: None,
            jobs: Vec::new(),
            undo_stack: Vec::new(),
            notice: None,
            palette: None,
            poll_task: None,
            pending_conflicts: VecDeque::new(),
            conflict_dialog: None,
        }
    }

    fn subscribe_to_pane(
        pane: &Entity<DirPane>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(pane, window, |this, pane, event, window, cx| match event {
            PaneEvent::Focus => this.set_active_pane(pane, cx),
            PaneEvent::Remove => this.remove_pane(&pane.clone(), window, cx),
            PaneEvent::Transfer { op, sources, dest } => {
                this.spawn_transfer(*op, sources.clone(), dest.clone(), window, cx)
            }
        })
    }

    fn set_active_pane(&mut self, pane: &Entity<DirPane>, cx: &mut Context<Self>) {
        if &self.active_pane != pane {
            self.active_pane = pane.clone();
            cx.notify();
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
        let pane = cx.new(|cx| DirPane::new(dir, view, window, cx));
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
        eprintln!("[pane] split {direction:?} -> {}", self.center.shape());
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
            window.focus(&fallback.focus_handle(cx), cx);
        }
        #[cfg(debug_assertions)]
        eprintln!("[pane] close -> {}", self.center.shape());
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
        match pane_transfer::spawn_job(spec) {
            Ok(handle) => {
                self.jobs.push(JobView {
                    handle,
                    dest_dir,
                    src_parents,
                    done: None,
                    errors: 0,
                    last_error: None,
                });
                self.ensure_polling(window, cx);
                cx.notify();
            }
            Err(err) => eprintln!("[pane] job rejected: {err}"),
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

        for job in &mut self.jobs {
            let job_id = job.handle.id();
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

        if !finished_dirs.is_empty() {
            for pane in &self.panes {
                let pane_dir = pane.read(cx).dir().to_path_buf();
                if finished_dirs.contains(&pane_dir) {
                    pane.update(cx, |pane, cx| pane.refresh(cx));
                }
            }
        }
        cx.notify();
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
            Err(err) => eprintln!("[pane] new folder failed: {err}"),
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
                        .map(|path| pane_transfer::trash(&path).map_err(|err| (path, err)))
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
                        match pane_transfer::restore(&item) {
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
            let names: Vec<String> = paths
                .iter()
                .filter(|p| p.parent() == Some(pane_dir.as_path()))
                .map(|p| file_label(p))
                .collect();
            if !names.is_empty() {
                pane.update(cx, |pane, _| pane.select_on_next_load(names));
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
        if self.palette.take().is_some() {
            window.focus(&self.active_pane.focus_handle(cx), cx);
            cx.notify();
            return;
        }

        // Captured before the palette takes focus: the action list, the key
        // bindings shown, and where a confirmed action dispatches all hang off
        // this handle.
        let origin = self.active_pane.focus_handle(cx);
        let palette = cx.new(|cx| CommandPalette::new(origin, window, cx));

        cx.subscribe_in(&palette, window, |this, _, _: &DismissEvent, window, cx| {
            this.palette = None;
            window.focus(&this.active_pane.focus_handle(cx), cx);
            cx.notify();
        })
        .detach();

        // The query field takes focus, not the palette shell: typing has to
        // reach the editor, and the palette's own bindings still fire because
        // the field is inside its dispatch path.
        let query_focus = palette.read(cx).query_focus(cx);
        window.focus(&query_focus, cx);
        self.palette = Some(palette);
        cx.notify();
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
        let bar_fill = colors.border_selected;

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
                let walk_complete = progress
                    .walk_complete
                    .load(std::sync::atomic::Ordering::Relaxed);
                let phase =
                    Phase::from_u8(progress.phase.load(std::sync::atomic::Ordering::Relaxed));

                let status = match (job.done, phase) {
                    (Some(Outcome::Cancelled), _) => "cancelled".to_string(),
                    (Some(_), _) if job.errors > 0 => {
                        job.last_error.clone().unwrap_or_else(|| "errors".into())
                    }
                    (Some(_), _) => "done".to_string(),
                    (None, Phase::Flushing) => "flushing to device…".to_string(),
                    (None, Phase::AwaitingConflict) => "waiting for answer…".to_string(),
                    _ => format!(
                        "{} / {}",
                        fs::format_size(bytes_done),
                        if walk_complete {
                            fs::format_size(bytes_total)
                        } else {
                            "…".to_string()
                        }
                    ),
                };

                let fraction = if walk_complete && bytes_total > 0 {
                    (bytes_done as f32 / bytes_total as f32).clamp(0., 1.)
                } else if job.done.is_some() {
                    1.
                } else {
                    0. // indeterminate until the walk finishes; no backwards bars
                };

                let is_done = job.done.is_some();
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
                            .h(px(4.))
                            .rounded_sm()
                            .bg(bar_bg)
                            .child(div().h_full().rounded_sm().bg(bar_fill).w(relative(fraction))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(320.))
                            .truncate()
                            .text_color(if job.errors > 0 { error_color } else { muted })
                            .child(status),
                    )
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
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::dismiss_jobs))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(|this, action, _, cx| this.delete(action, cx)))
            .on_action(cx.listener(|this, action, _, cx| this.undo(action, cx)))
            .on_action(cx.listener(Self::toggle_palette))
            .child(self.center.render(&self.active_pane, window, cx))
            .when(!self.jobs.is_empty() || self.notice.is_some(), |el| {
                el.child(self.render_job_strip(cx))
            })
            .when_some(self.palette.clone(), |el, palette| {
                el.child(
                    div()
                        .occlude()
                        .absolute()
                        .inset_0()
                        .bg(hsla(0., 0., 0., 0.45))
                        .flex()
                        .justify_center()
                        // Anchored near the top rather than centred: the list
                        // grows downward, so a centred modal would shift under
                        // the cursor as results narrow.
                        .pt(px(80.))
                        .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| {
                            this.palette = None;
                            window.focus(&this.active_pane.focus_handle(cx), cx);
                            cx.notify();
                        }))
                        .child(palette),
                )
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
