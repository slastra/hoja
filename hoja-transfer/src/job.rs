//! Job lifecycle: spec validation, the worker thread, the walk-while-copying
//! loop, and the per-file tier ladder.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::conflict::{ConflictState, Resolution};
use crate::copy::{self, CopyMechanism, CopyOutcome};
use crate::dispatch::MountPairCache;
use crate::events::{
    ConflictChoice, Event, JobSummary, Operation, Outcome, Stage, TierStats, TransferError, Undone,
};
use crate::meta;
use crate::sys::{self, MountKey};
use crate::trash::{TrashDir, TrashedItem};

/// How many changes a job may record before it stops offering to undo itself.
///
/// Reached only by a copy that merged into an existing tree, since anything
/// landing on a fresh name collapses to one record. Sized like
/// `MAX_RETAINED_FAILURES` in the UI: high enough that no ordinary transfer
/// approaches it, low enough that the log cannot become the reason a machine
/// runs out of memory.
const MAX_UNDO_RECORDS: usize = 100_000;

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub op: Operation,
    /// Top-level items (files or directories); each lands at `dest_dir/<file_name>`.
    pub sources: Vec<PathBuf>,
    /// Must be an existing directory.
    pub dest_dir: PathBuf,
    pub policy: JobPolicy,
}

#[derive(Debug, Clone, Default)]
pub struct JobPolicy {
    /// `Some` = pre-decided, no Conflict events. `None` = ask the UI per conflict
    /// until an apply-to-all answer makes it sticky.
    pub conflict: Option<ConflictChoice>,
    /// Default false: errors queue, the job continues.
    pub abort_on_first_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Starting = 0,
    Transferring = 1,
    AwaitingConflict = 2,
    Flushing = 3,
    Finished = 4,
    /// Counting the tree, before a byte is copied. Appended rather than
    /// inserted: the value crosses to the UI as a raw u8.
    Scanning = 5,
}

impl Phase {
    pub fn from_u8(v: u8) -> Phase {
        match v {
            0 => Phase::Starting,
            1 => Phase::Transferring,
            2 => Phase::AwaitingConflict,
            3 => Phase::Flushing,
            5 => Phase::Scanning,
            _ => Phase::Finished,
        }
    }
}

/// All-atomic; safe to sample from any thread at any cadence. Totals are settled
/// by the scan before the transfer starts, except where the scan is skipped (see
/// `should_scan`), in which case they grow as the walk finds things:
/// `walk_complete` tells the UI when the denominator
/// is final, so it can render an indeterminate bar before that.
#[derive(Debug, Default)]
pub struct Progress {
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
    pub walk_complete: AtomicBool,
    pub phase: AtomicU8,
    pub current_file: Mutex<Option<PathBuf>>,
    /// Whether the worker has actually stopped, which is not the same question
    /// as whether it was asked to.
    ///
    /// Pause is honoured between files, so a 4 GB file already in flight keeps
    /// copying for a while after the button is pressed. A UI reading the
    /// request back would say "paused" over a bar that is visibly still
    /// moving; this says "paused" when the bytes have in fact stopped, and lets
    /// the interval between the two read as "pausing…".
    ///
    /// Deliberately not a `Phase`: a job can be parked while scanning or while
    /// transferring, and it resumes into whichever it left.
    pub paused: AtomicBool,
}

impl Progress {
    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    fn set_current(&self, path: &Path) {
        *self.current_file.lock().unwrap() = Some(path.to_path_buf());
    }
}

pub struct JobHandle {
    id: JobId,
    label: String,
    progress: Arc<Progress>,
    events: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl JobHandle {
    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn progress(&self) -> &Arc<Progress> {
        &self.progress
    }

    /// Non-blocking drain; the UI calls this on its poll timer.
    pub fn try_recv_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Sets the flag; the worker notices within ~100ms, including while blocked
    /// on a conflict prompt.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Ask the worker to stop between files, or to carry on.
    ///
    /// A request, not an acknowledgement: a file already in flight finishes
    /// first. `progress().paused` says when it has actually stopped.
    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
}

impl Drop for JobHandle {
    /// Stop the worker rather than abandoning it.
    ///
    /// Every `events.send` in the worker discards its result, so a dropped
    /// receiver is invisible to it and it would run a whole transfer nobody is
    /// listening to. A *paused* one is worse: it sits in `check_pause_cancel`
    /// waiting on a flag whose only setter has just been dropped, so it never
    /// comes back at all.
    ///
    /// Signals and returns. Joining here would block whichever thread let the
    /// handle go — for the UI, that is the window closing on a 200 GB copy.
    fn drop(&mut self) {
        self.cancel();
    }
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Validate cheaply, spawn one worker thread, return immediately.
pub fn spawn_job(spec: JobSpec) -> std::io::Result<JobHandle> {
    if !spec.dest_dir.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "destination is not a directory: {}",
                spec.dest_dir.display()
            ),
        ));
    }
    if spec.sources.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no sources",
        ));
    }
    // A directory copied into its own descendant walks a tree that grows as it
    // writes. The rename fast path fails safely with EINVAL, but the fallback
    // would not, so refuse here rather than at each call site: drag-and-drop
    // makes this one gesture, and cut-then-paste-inside-itself reaches it too.
    if let Some(source) = spec
        .sources
        .iter()
        .find(|source| spec.dest_dir.starts_with(source))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "destination {} is inside source {}",
                spec.dest_dir.display(),
                source.display()
            ),
        ));
    }

    let id = JobId(NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
    let label = {
        let verb = match spec.op {
            Operation::Copy => "Copying",
            Operation::Move => "Moving",
        };
        let what = if spec.sources.len() == 1 {
            spec.sources[0]
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "1 item".into())
        } else {
            format!("{} items", spec.sources.len())
        };
        let to = spec
            .dest_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| spec.dest_dir.display().to_string());
        format!("{verb} {what} → {to}")
    };

    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let (events_tx, events_rx) = mpsc::channel();

    let thread = std::thread::Builder::new()
        .name(format!("hoja-transfer-{}", id.0))
        .spawn({
            let progress = progress.clone();
            let cancel = cancel.clone();
            let pause = pause.clone();
            move || {
                let mut worker = Worker::new(spec, progress, events_tx, cancel, pause);
                worker.run();
            }
        })?;

    Ok(JobHandle {
        id,
        label,
        progress,
        events: events_rx,
        cancel,
        pause,
        thread: Some(thread),
    })
}

/// Put a transfer back, and report progress the same way it did.
///
/// A `JobHandle` like any other, deliberately: the strip, the failure report,
/// the desktop notification and the polling loop all work on one already, and
/// undoing two hundred thousand files is exactly the kind of thing that needs
/// a progress bar and a cancel button. `label` is the transfer's, so the row
/// says which one is being taken back.
///
/// The summary's `undone` carries what it could *not* reverse rather than what
/// it did — a file changed since, or one whose previous contents were never
/// kept — so the caller can put those back on its undo stack and let a second
/// press try again, which is what the delete undo already does.
pub fn spawn_undo(label: String, records: Vec<Undone>) -> std::io::Result<JobHandle> {
    let id = JobId(NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let (events_tx, events_rx) = mpsc::channel();

    // A spec only so `Worker` can be built: nothing here walks a source or
    // resolves a destination. The conflict policy matters, though — the
    // copy-back of a cross-filesystem move goes through the same ladder as a
    // forward one, and there is nobody to ask.
    let spec = JobSpec {
        op: Operation::Move,
        sources: Vec::new(),
        dest_dir: PathBuf::new(),
        policy: JobPolicy {
            conflict: Some(ConflictChoice::Skip),
            abort_on_first_error: false,
        },
    };

    let thread = std::thread::Builder::new()
        .name(format!("hoja-undo-{}", id.0))
        .spawn({
            let progress = progress.clone();
            let cancel = cancel.clone();
            let pause = pause.clone();
            move || {
                let mut worker = Worker::new(spec, progress, events_tx, cancel, pause);
                worker.run_undo(records);
            }
        })?;

    Ok(JobHandle {
        id,
        label: format!("Undoing {label}"),
        progress,
        events: events_rx,
        cancel,
        pause,
        thread: Some(thread),
    })
}

// ---------------------------------------------------------------------------

/// Per-file unwinding: an error queues and the walk continues; Cancelled and
/// Fatal unwind the whole job.
enum Step {
    Ok,
    Cancelled,
    Fatal,
}

struct Worker {
    spec: JobSpec,
    progress: Arc<Progress>,
    events: mpsc::Sender<Event>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    caps: MountPairCache,
    conflicts: ConflictState,
    /// (dev, ino) of multi-link sources → first destination written.
    hardlinks: HashMap<(u64, u64), PathBuf>,
    stats: TierStats,
    errors: Vec<(PathBuf, TransferError)>,
    files_copied: u64,
    files_skipped: u64,
    dest_mount: Option<MountKey>,
    /// The scan ran and finished, so the totals are final and the transfer must
    /// not add to them again.
    scanned: bool,
    /// Reused by every read/write fallback in the job: see `copy_extent`.
    copy_buf: Vec<u8>,
    /// `st_dev` → mount key, so the statx behind `mount_key` happens once per
    /// filesystem rather than once per file. Keying on `st_dev` is sound for
    /// what the answer is used for: it decides whether rename and reflink are
    /// worth attempting, and two bind mounts of one filesystem (the case
    /// `MNT_ID` exists to tell apart) share a superblock and so share both
    /// answers.
    mount_keys: HashMap<u64, MountKey>,
    /// What this job did, for undoing it. See `Undone`.
    undone: Vec<Undone>,
    /// Whether the log is still the whole story. Cleared by the cap, and by
    /// anything this job did that it cannot take back — today that is an
    /// overwrite, which unlinks what was there.
    undoable: bool,
    /// The trash for each filesystem this job has had to displace something
    /// on, by device. `None` against a device means looked for and not found.
    ///
    /// Keyed rather than kept as one, because "a job writes to one
    /// filesystem" is false the moment its destination tree contains a mount
    /// point. One slot meant every later file was offered a trash on the wrong
    /// volume, where the rename fails EXDEV — so the overwrite went ahead
    /// having binned nothing, and the file it replaced was gone.
    trash_dirs: HashMap<u64, Option<TrashDir>>,
    /// Basename → the counter that last claimed a trash name for it.
    trash_names: HashMap<String, u32>,
    /// One warning per job, not one per file: a sync over a filesystem with no
    /// trash would otherwise raise thousands of identical ones.
    warned_no_trash: bool,
    /// Whether everything being written now sits under a directory this job
    /// created, in which case that one record already accounts for it.
    ///
    /// This is what makes a fresh copy of a hundred thousand files one record
    /// instead of a hundred thousand: `process_dir` sets it on the way into a
    /// directory it had to make, and puts it back on the way out, so a merge
    /// two levels down still records per file while the untouched branches
    /// beside it collapse.
    claimed: bool,
}

impl Worker {
    fn new(
        spec: JobSpec,
        progress: Arc<Progress>,
        events: mpsc::Sender<Event>,
        cancel: Arc<AtomicBool>,
        pause: Arc<AtomicBool>,
    ) -> Self {
        let conflicts = ConflictState::new(spec.policy.conflict, events.clone(), cancel.clone());
        Self {
            spec,
            progress,
            events,
            cancel,
            pause,
            caps: MountPairCache::default(),
            conflicts,
            hardlinks: HashMap::new(),
            stats: TierStats::default(),
            errors: Vec::new(),
            files_copied: 0,
            files_skipped: 0,
            dest_mount: None,
            scanned: false,
            copy_buf: Vec::new(),
            mount_keys: HashMap::new(),
            undone: Vec::new(),
            undoable: true,
            trash_dirs: HashMap::new(),
            trash_names: HashMap::new(),
            warned_no_trash: false,
            claimed: false,
        }
    }

    /// Note something that will need reversing, unless a record already
    /// covers it.
    fn undone(&mut self, record: Undone) {
        if self.claimed || !self.undoable {
            return;
        }
        if self.undone.len() >= MAX_UNDO_RECORDS {
            self.cannot_undo();
            return;
        }
        self.undone.push(record);
    }

    /// Record what each directory this job made looked like when it stopped.
    ///
    /// At the end rather than at creation, because at creation it is empty and
    /// the job is about to fill it. What this captures is the state undo is
    /// entitled to remove; anything added afterwards moves the mtime and undo
    /// declines. A handful of stats, since only directories the job actually
    /// made are in here.
    fn stamp_created_dirs(&mut self) {
        for record in &mut self.undone {
            if let Undone::CreatedDir {
                path,
                whole: true,
                mtime,
            } = record
            {
                *mtime = std::fs::symlink_metadata(&*path)
                    .ok()
                    .and_then(|m| m.modified().ok());
            }
        }
    }

    /// Give up on undoing this job, and drop what was recorded so far.
    ///
    /// Dropped whole rather than kept: half a log undoes half a transfer and
    /// reports success, which is the one outcome worse than saying plainly
    /// that it cannot be undone.
    fn cannot_undo(&mut self) {
        self.undoable = false;
        self.undone = Vec::new();
    }

    /// Whether to count the tree before transferring it.
    ///
    /// Worth it whenever bytes will actually be copied: the walk costs about a
    /// second and a half for 86,000 files against a copy of the same tree that
    /// costs minutes, and it buys a real denominator, without it the progress
    /// label reads `1.2 MB / …` for the entire job and the bar never leaves
    /// zero, because the walk only completes when the copy does.
    ///
    /// Not worth it for a move that stays on one mount: `process_dir` renames
    /// the whole subtree in a single call, so scanning would be the only slow
    /// part of an instant operation. The mount check is one statx per source.
    fn should_scan(&self) -> bool {
        if self.spec.op != Operation::Move {
            return true;
        }
        let Some(dest_mount) = self.dest_mount else {
            return true;
        };
        !self
            .spec
            .sources
            .iter()
            .all(|src| sys::mount_key(src).is_ok_and(|key| key == dest_mount))
    }

    /// Count what the transfer is about to do. Must agree with what the
    /// transfer then reports as done, or the bar cannot reach the end.
    ///
    /// Errors are swallowed rather than queued: this is a counting pass, and
    /// the transfer that follows will hit the same path and report it properly.
    /// A miscount is not worth a duplicate error.
    fn scan(&mut self, src: &Path) -> Step {
        if self.check_pause_cancel() {
            return Step::Cancelled;
        }
        let Ok(meta) = std::fs::symlink_metadata(src) else {
            return Step::Ok;
        };

        // symlink_metadata, not metadata: a symlink is recreated, never read, so
        // it is one file and no bytes. Following it here would add its target's
        // size to a total that the transfer never counts towards done, and
        // node_modules is full of them, so the bar would stop short every time.
        if meta.file_type().is_symlink() {
            self.progress.files_total.fetch_add(1, Ordering::Relaxed);
            return Step::Ok;
        }

        if meta.is_dir() {
            let Ok(entries) = std::fs::read_dir(src) else {
                return Step::Ok;
            };
            for entry in entries.filter_map(Result::ok) {
                if matches!(self.scan(&entry.path()), Step::Cancelled) {
                    return Step::Cancelled;
                }
            }
            return Step::Ok;
        }

        if meta.is_file() {
            self.progress.files_total.fetch_add(1, Ordering::Relaxed);
            self.progress
                .bytes_total
                .fetch_add(meta.len(), Ordering::Relaxed);
        }
        // Special files are neither counted nor copied; the transfer refuses them.
        Step::Ok
    }

    fn run(&mut self) {
        self.dest_mount = sys::mount_key(&self.spec.dest_dir).ok();
        let mut cancelled = false;

        if self.should_scan() {
            self.progress.set_phase(Phase::Scanning);
            for src in self.spec.sources.clone() {
                if matches!(self.scan(&src), Step::Cancelled) {
                    cancelled = true;
                    break;
                }
            }
            // Only claim the totals are settled if the scan actually finished.
            self.scanned = !cancelled;
            if self.scanned {
                // The denominator is final here, which is the whole point: the
                // UI can draw a real bar from the first copied byte.
                self.progress.walk_complete.store(true, Ordering::Relaxed);
            }
        }

        if !cancelled {
            self.progress.set_phase(Phase::Transferring);
            for src in self.spec.sources.clone() {
                let Some(name) = src.file_name() else {
                    self.queue_error(&src, Stage::Walk, std::io::Error::other("no file name"));
                    continue;
                };
                let dest = self.spec.dest_dir.join(name);
                match self.process_item(&src, &dest, self.scanned) {
                    Step::Ok => {}
                    Step::Cancelled => {
                        cancelled = true;
                        break;
                    }
                    Step::Fatal => break,
                }
            }
        }
        self.progress.walk_complete.store(true, Ordering::Relaxed);

        // Honest completion on removable media: the data is on the device before
        // we say we're done. An fd on any file of that filesystem works.
        if !cancelled && sys::is_removable(&self.spec.dest_dir) {
            self.progress.set_phase(Phase::Flushing);
            match File::open(&self.spec.dest_dir) {
                Ok(dir_fd) => {
                    if let Err(err) = rustix::fs::syncfs(&dir_fd) {
                        self.queue_error(&self.spec.dest_dir.clone(), Stage::Sync, err.into());
                    }
                }
                Err(err) => self.queue_error(&self.spec.dest_dir.clone(), Stage::Sync, err),
            }
        }

        self.progress.set_phase(Phase::Finished);
        // After the last write, so what each created directory holds now is
        // what undo is entitled to take back.
        self.stamp_created_dirs();
        let outcome = if cancelled {
            Outcome::Cancelled
        } else if self.errors.is_empty() {
            Outcome::Completed
        } else {
            Outcome::CompletedWithErrors
        };
        let summary = JobSummary {
            outcome,
            errors: std::mem::take(&mut self.errors),
            files_copied: self.files_copied,
            files_skipped: self.files_skipped,
            bytes_copied: self.progress.bytes_done.load(Ordering::Relaxed),
            stats: self.stats,
            undone: std::mem::take(&mut self.undone),
            undoable: self.undoable,
        };
        let _ = self.events.send(Event::Done(summary));
    }

    /// Walk the log backwards, reversing what it can.
    ///
    /// Backwards because the records of one overwrite are the old file
    /// displaced and then the new one created, and putting the old one back
    /// has to happen after the new one is out of the way — `restore` refuses
    /// to clobber, so the other order would simply fail.
    fn run_undo(&mut self, records: Vec<Undone>) {
        self.progress
            .files_total
            .store(records.len() as u64, Ordering::Relaxed);
        // Nothing to walk: the denominator is known before the first step, so
        // the bar is real from the start.
        self.progress.walk_complete.store(true, Ordering::Relaxed);
        self.progress.set_phase(Phase::Transferring);

        let mut cancelled = false;
        let mut outstanding = Vec::new();
        let mut left = records.into_iter().rev();
        for record in left.by_ref() {
            if self.check_pause_cancel() {
                cancelled = true;
                outstanding.push(record);
                break;
            }
            if !self.undo_one(&record) {
                outstanding.push(record);
            }
            self.progress.files_done.fetch_add(1, Ordering::Relaxed);
        }
        // Everything the loop never reached is still owed. Breaking out of a
        // consuming iterator drops the rest of it, so cancelling halfway
        // through undoing two hundred thousand files used to lose the record
        // of the hundred thousand not yet reversed — leaving them at the
        // destination with nothing anywhere that knew they were there.
        outstanding.extend(left);

        self.progress.set_phase(Phase::Finished);
        // Back into the order they were recorded in, so what goes onto the
        // stack can be replayed by a second press exactly as this one was.
        outstanding.reverse();
        let summary = JobSummary {
            outcome: if cancelled {
                Outcome::Cancelled
            } else if self.errors.is_empty() {
                Outcome::Completed
            } else {
                Outcome::CompletedWithErrors
            },
            errors: std::mem::take(&mut self.errors),
            files_copied: self.files_copied,
            files_skipped: self.files_skipped,
            bytes_copied: self.progress.bytes_done.load(Ordering::Relaxed),
            stats: self.stats,
            undone: outstanding,
            undoable: false,
        };
        let _ = self.events.send(Event::Done(summary));
    }

    /// Reverse one record. `false` means it is still outstanding.
    fn undo_one(&mut self, record: &Undone) -> bool {
        match record {
            Undone::Displaced(item) => match crate::trash::restore(item) {
                Ok(()) => {
                    self.files_copied += 1;
                    true
                }
                Err(err) => {
                    self.queue_error(&item.original, Stage::Rename, err);
                    false
                }
            },
            Undone::Lost(path) => {
                self.queue_error(
                    path,
                    Stage::Write,
                    std::io::Error::other(
                        "this was replaced on a filesystem with no trash, so what was here was not kept",
                    ),
                );
                false
            }
            Undone::Renamed { from, to, dev, ino } => {
                if !self.still_the_same(to, *dev, *ino) {
                    return false;
                }
                match sys::rename_no_replace(to, from) {
                    Ok(()) => {
                        self.stats.renames += 1;
                        self.files_copied += 1;
                        true
                    }
                    Err(err) => {
                        self.queue_error(to, Stage::Rename, err);
                        false
                    }
                }
            }
            Undone::RemovedDir(path) => match std::fs::create_dir_all(path) {
                Ok(()) => true,
                Err(err) => {
                    self.queue_error(path, Stage::CreateDir, err);
                    false
                }
            },
            Undone::CreatedDir { path, whole, mtime } => {
                let Ok(meta) = std::fs::symlink_metadata(path) else {
                    // Already gone. Nothing to take back, and nothing wrong.
                    return true;
                };
                if !*whole {
                    // Its children had records of their own and have already
                    // gone back, so this should be empty. `remove_dir` refuses
                    // if it is not, which is exactly the check wanted: whatever
                    // is still in there is not this job's to remove.
                    return match std::fs::remove_dir(path) {
                        Ok(()) => true,
                        Err(err) => {
                            self.queue_error(path, Stage::DeleteSource, err);
                            false
                        }
                    };
                }
                // This record stands for the whole subtree, so removing it
                // would take anything added since with it. A directory's mtime
                // moves when an entry is added, removed or renamed.
                if meta.modified().ok() != *mtime {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other(
                            "this has been added to or emptied since the transfer",
                        ),
                    );
                    return false;
                }
                self.take_back(path, None)
            }
            Undone::Created {
                path,
                from,
                dev,
                ino,
                len,
                mtime,
            } => {
                let Ok(meta) = std::fs::symlink_metadata(path) else {
                    // Already gone. Nothing to take back, and nothing wrong.
                    return true;
                };
                if meta.dev() != *dev || meta.ino() != *ino {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other("something else is here now"),
                    );
                    return false;
                }
                if meta.len() != *len || meta.modified().ok() != *mtime {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other("this has been edited since the transfer"),
                    );
                    return false;
                }
                self.take_back(path, from.as_deref())
            }
        }
    }

    /// Whether `path` is still the thing the record was written about.
    fn still_the_same(&mut self, path: &Path, dev: u64, ino: u64) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.dev() == dev && meta.ino() == ino => true,
            Ok(_) => {
                self.queue_error(
                    path,
                    Stage::Write,
                    std::io::Error::other("something else is here now"),
                );
                false
            }
            Err(err) => {
                self.queue_error(path, Stage::Walk, err);
                false
            }
        }
    }

    /// Remove something this job's transfer put here, or move it back where it
    /// came from.
    ///
    /// `home` is set for a file whose source was deleted after it landed — a
    /// move across filesystems — and then this is that move in reverse, at the
    /// price it was bought for. Otherwise the source is still where it was and
    /// taking the copy away is the whole of it.
    ///
    /// Taking it away means the trash, not `unlink`. Undo is a guess about
    /// what someone wanted, and a guess that destroys files is one they cannot
    /// take back.
    fn take_back(&mut self, path: &Path, home: Option<&Path>) -> bool {
        if let Some(home) = home {
            match sys::rename_no_replace(path, home) {
                Ok(()) => {
                    self.stats.renames += 1;
                    self.files_copied += 1;
                    return true;
                }
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                    // What it was: a copy across a boundary and then a delete.
                    // Reversing it is the same again the other way, through
                    // the same ladder, which is why this is a Worker at all.
                    //
                    // Checked first, because unlike the `rename_no_replace`
                    // above, `copy_file_inner` ends in a plain rename and would
                    // clobber. Something new at the original path is somebody
                    // else's file, and putting ours back is not worth taking
                    // theirs.
                    if std::fs::symlink_metadata(home).is_ok() {
                        self.queue_error(
                            home,
                            Stage::Write,
                            std::io::Error::other("something else is here now"),
                        );
                        return false;
                    }
                    let Ok(meta) = std::fs::symlink_metadata(path) else {
                        return true;
                    };
                    return matches!(
                        self.copy_file_inner(path, home, &meta, true, false),
                        Step::Ok
                    );
                }
                Err(err) => {
                    self.queue_error(path, Stage::Rename, err);
                    return false;
                }
            }
        }

        let device = std::fs::symlink_metadata(path).map(|m| m.dev()).ok();
        if let Some(device) = device
            && !self.trash_dirs.contains_key(&device)
        {
            self.trash_dirs
                .insert(device, TrashDir::for_path(path).ok());
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let start = self.trash_names.get(&name).copied().unwrap_or(1);
        let put = device
            .and_then(|device| self.trash_dirs.get(&device))
            .and_then(Option::as_ref)
            .map(|bin| bin.put(path, start));
        match put {
            Some(Ok((_, attempt))) => {
                self.trash_names.insert(name, attempt);
                self.files_copied += 1;
                true
            }
            Some(Err(err)) => {
                self.queue_error(path, Stage::Rename, err);
                false
            }
            None => {
                // Refused, not deleted. Undo is a guess about what somebody
                // wanted, and the whole reason it moves files to the trash is
                // so a wrong guess costs nothing. Falling back to unlinking
                // where no trash exists made the one case that cannot be taken
                // back the one case with no safety net — and for a directory
                // that meant `remove_dir_all` over a subtree this job had
                // stopped keeping records of.
                self.queue_error(
                    path,
                    Stage::DeleteSource,
                    std::io::Error::other(
                        "nothing here can hold a trash, so this was left rather than deleted",
                    ),
                );
                false
            }
        }
    }

    fn check_pause_cancel(&self) -> bool {
        // The store is inside the branch because this runs before every file
        // and the answer is almost always "not paused", which should stay one
        // relaxed load and nothing else.
        if self.pause.load(Ordering::Relaxed) {
            self.progress.paused.store(true, Ordering::Relaxed);
            while self.pause.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            self.progress.paused.store(false, Ordering::Relaxed);
        }
        self.cancel.load(Ordering::Relaxed)
    }

    fn queue_error(&mut self, path: &Path, stage: Stage, source: std::io::Error) {
        let error = TransferError { stage, source };
        let _ = self.events.send(Event::FileError {
            path: path.to_path_buf(),
            error: TransferError {
                stage,
                source: std::io::Error::new(error.source.kind(), error.source.to_string()),
            },
        });
        self.errors.push((path.to_path_buf(), error));
    }

    fn warn(&self, path: &Path, detail: String) {
        let _ = self.events.send(Event::Warning {
            path: path.to_path_buf(),
            detail,
        });
    }

    /// Depth-first item processor. Depth is bounded by filesystem path limits,
    /// so recursion is safe here.
    fn process_item(&mut self, src: &Path, dest: &Path, counted: bool) -> Step {
        if self.check_pause_cancel() {
            return Step::Cancelled;
        }

        let src_meta = match std::fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(err) => {
                self.queue_error(src, Stage::Walk, err);
                return self.continue_or_fatal();
            }
        };

        if src_meta.file_type().is_symlink() {
            return self.transfer_symlink(src, dest, counted);
        }
        if src_meta.is_dir() {
            return self.process_dir(src, dest, &src_meta);
        }
        if src_meta.is_file() {
            if !counted && !self.scanned {
                self.progress.files_total.fetch_add(1, Ordering::Relaxed);
                self.progress
                    .bytes_total
                    .fetch_add(src_meta.len(), Ordering::Relaxed);
            }
            return self.transfer_file(src, dest, &src_meta);
        }

        // FIFOs, sockets, devices: refuse politely rather than block forever
        // reading a FIFO.
        self.queue_error(
            src,
            Stage::Open,
            std::io::Error::other("special files are not copied"),
        );
        self.continue_or_fatal()
    }

    /// `mount_key` for a path whose metadata we already hold.
    fn src_mount_key(&mut self, path: &Path, meta: &std::fs::Metadata) -> Option<MountKey> {
        if let Some(key) = self.mount_keys.get(&meta.dev()) {
            return Some(*key);
        }
        let key = sys::mount_key(path).ok()?;
        self.mount_keys.insert(meta.dev(), key);
        Some(key)
    }

    fn continue_or_fatal(&self) -> Step {
        if self.spec.policy.abort_on_first_error {
            Step::Fatal
        } else {
            Step::Ok
        }
    }

    fn transfer_symlink(&mut self, src: &Path, dest: &Path, counted: bool) -> Step {
        if !counted && !self.scanned {
            self.progress.files_total.fetch_add(1, Ordering::Relaxed);
        }
        let (dest, replacing) = match self.resolve_dest(src, dest) {
            DestPlan::Proceed { path, replacing } => (path, replacing),
            DestPlan::Skip => {
                self.files_skipped += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                return Step::Ok;
            }
            DestPlan::Cancel => return Step::Cancelled,
        };
        // Nothing is written before this point, so this is the last moment,
        // and the name has to be clear either way: `symlink()` has no replace
        // semantics. `displace` moves the old entry to the trash and the
        // unlink then finds nothing; where it could not, the unlink is what
        // actually clears the way, which is why it runs on `replacing` rather
        // than on whether the trash accepted it.
        if replacing {
            self.displace(&dest);
            let _ = std::fs::remove_file(&dest);
        }
        match meta::copy_symlink(src, &dest) {
            Ok(()) => {
                self.stats.symlinks += 1;
                if let Ok(meta) = std::fs::symlink_metadata(&dest) {
                    self.undone(Undone::Created {
                        path: dest.to_path_buf(),
                        from: (self.spec.op == Operation::Move).then(|| src.to_path_buf()),
                        dev: meta.dev(),
                        ino: meta.ino(),
                        len: meta.len(),
                        mtime: meta.modified().ok(),
                    });
                }
                self.files_copied += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                if self.spec.op == Operation::Move
                    && let Err(err) = std::fs::remove_file(src)
                {
                    self.queue_error(src, Stage::DeleteSource, err);
                }
                Step::Ok
            }
            Err(err) => {
                self.queue_error(src, Stage::Symlink, err);
                self.continue_or_fatal()
            }
        }
    }

    fn process_dir(&mut self, src: &Path, dest: &Path, src_meta: &std::fs::Metadata) -> Step {
        // Move fast path: a same-mount directory rename moves the whole subtree
        // in one atomic call. NOREPLACE both expresses "only if absent" and
        // closes the window between testing and renaming.
        if self.spec.op == Operation::Move
            && let (Some(src_key), Some(dst_key)) =
                (self.src_mount_key(src, src_meta), self.dest_mount)
            && self.caps.rename_worth_trying(src_key, dst_key)
        {
            match sys::rename_no_replace(src, dest) {
                Ok(()) => {
                    self.stats.renames += 1;
                    // One call moved the whole subtree, and one call moves it
                    // back. Never walk this to record it per file.
                    self.undone(Undone::Renamed {
                        from: src.to_path_buf(),
                        to: dest.to_path_buf(),
                        dev: src_meta.dev(),
                        ino: src_meta.ino(),
                    });
                    if !self.scanned {
                        self.progress.files_total.fetch_add(1, Ordering::Relaxed);
                    }
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    self.files_copied += 1;
                    return Step::Ok;
                }
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                    self.caps.mark_rename_failed(src_key, dst_key);
                }
                // Something is already at dest: fall through and merge into it.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    self.queue_error(src, Stage::Rename, err);
                    return self.continue_or_fatal();
                }
            }
        }

        // Existing directory at dest = merge; only create when absent.
        let made_it = !dest.is_dir();
        if made_it && let Err(err) = std::fs::create_dir(dest) {
            self.queue_error(dest, Stage::CreateDir, err);
            return self.continue_or_fatal();
        }
        // Everything below a directory this job made is accounted for by the
        // directory, so the children record nothing. Put back on the way out:
        // a sibling that merges into an existing tree still needs its own.
        let outer_claim = self.claimed;
        if made_it {
            // Only a copy may let the directory stand for its contents. A
            // move deletes each source as it goes, and removing this directory
            // would not put any of them back — its children have to speak for
            // themselves, and say where they came from.
            let whole = self.spec.op == Operation::Copy;
            self.undone(Undone::CreatedDir {
                path: dest.to_path_buf(),
                whole,
                // Stamped at the end of the job, once nothing more will be
                // written into it.
                mtime: None,
            });
            if whole {
                self.claimed = true;
            }
        }
        let step = self.process_dir_children(src, dest, src_meta);
        self.claimed = outer_claim;
        step
    }

    /// The body of `process_dir` once the destination directory exists, split
    /// out so the claim above it is restored on every path out of it.
    fn process_dir_children(
        &mut self,
        src: &Path,
        dest: &Path,
        src_meta: &std::fs::Metadata,
    ) -> Step {
        // Enumerate children first so totals grow ahead of processing (this IS
        // the one walk: gate counters for M3 accumulate on these same adds).
        let entries = match std::fs::read_dir(src) {
            Ok(iter) => {
                let mut v: Vec<_> = iter.filter_map(Result::ok).collect();
                v.sort_by_key(|e| e.file_name());
                v
            }
            Err(err) => {
                self.queue_error(src, Stage::Walk, err);
                return self.continue_or_fatal();
            }
        };
        if !self.scanned {
            for entry in &entries {
                if let Ok(m) = entry.metadata()
                    && m.is_file()
                {
                    self.progress.files_total.fetch_add(1, Ordering::Relaxed);
                    self.progress
                        .bytes_total
                        .fetch_add(m.len(), Ordering::Relaxed);
                }
            }
        }

        let mut child_failed = false;
        for entry in entries {
            let child_src = entry.path();
            let child_dest = dest.join(entry.file_name());
            match self.process_item(&child_src, &child_dest, true) {
                Step::Ok => {}
                Step::Cancelled => return Step::Cancelled,
                Step::Fatal => return Step::Fatal,
            }
            // Track whether anything under this dir errored, for move cleanup.
            if self.errors.iter().any(|(p, _)| p.starts_with(&child_src)) {
                child_failed = true;
            }
        }

        // Metadata after children: writes inside would clobber the times.
        let outcome = meta::apply_dir_meta(src_meta, dest);
        for w in outcome.warnings {
            self.warn(dest, w);
        }

        // Move: remove the now-empty source dir, but never when a child failed,
        // that would orphan whatever is still inside.
        if self.spec.op == Operation::Move && !child_failed {
            match std::fs::remove_dir(src) {
                // Recorded after the children, so replaying backwards makes
                // the directory again before trying to move anything into it.
                // Without this every child's `Renamed` failed ENOENT on a
                // parent that no longer existed, and a merged move could not
                // be undone at all.
                Ok(()) => self.undone(Undone::RemovedDir(src.to_path_buf())),
                Err(err) => self.queue_error(src, Stage::DeleteSource, err),
            }
        }
        Step::Ok
    }

    fn transfer_file(&mut self, src: &Path, dest: &Path, src_meta: &std::fs::Metadata) -> Step {
        self.progress.set_current(src);

        // Tier 0: move via rename.
        if self.spec.op == Operation::Move
            && let (Some(src_key), Some(dst_key)) =
                (self.src_mount_key(src, src_meta), self.dest_mount)
            && self.caps.rename_worth_trying(src_key, dst_key)
        {
            // rename() clobbers silently, so conflicts resolve BEFORE the
            // attempt, and the attempt itself refuses to replace, so a file
            // appearing in between is re-resolved rather than destroyed.
            let (mut planned, replacing) = match self.resolve_dest(src, dest) {
                DestPlan::Proceed { path, replacing } => (path, replacing),
                DestPlan::Skip => {
                    self.files_skipped += 1;
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    return Step::Ok;
                }
                DestPlan::Cancel => return Step::Cancelled,
            };
            // The rename below is the destructive act and there is nothing to
            // undo before it, so the old file moves out of the way here.
            // NOREPLACE would otherwise refuse the replacement the user asked
            // for, hence clearing the name whether or not the trash took it.
            if replacing && std::fs::symlink_metadata(&planned).is_ok() {
                self.displace(&planned);
                let _ = std::fs::remove_file(&planned);
            }
            let mut renamed = sys::rename_no_replace(src, &planned);
            if matches!(&renamed, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists) {
                // Raced: ask again against the destination that now exists.
                match self.resolve_dest(src, &planned) {
                    DestPlan::Proceed {
                        path: d,
                        replacing: again,
                    } => {
                        planned = d;
                        if again && std::fs::symlink_metadata(&planned).is_ok() {
                            self.displace(&planned);
                            let _ = std::fs::remove_file(&planned);
                        }
                        renamed = sys::rename_no_replace(src, &planned);
                    }
                    DestPlan::Skip => {
                        self.files_skipped += 1;
                        self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                        return Step::Ok;
                    }
                    DestPlan::Cancel => return Step::Cancelled,
                }
            }
            match renamed {
                Ok(()) => {
                    self.stats.renames += 1;
                    self.undone(Undone::Renamed {
                        from: src.to_path_buf(),
                        to: planned.clone(),
                        dev: src_meta.dev(),
                        ino: src_meta.ino(),
                    });
                    self.files_copied += 1;
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    self.progress
                        .bytes_done
                        .fetch_add(src_meta.len(), Ordering::Relaxed);
                    return Step::Ok;
                }
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                    self.caps.mark_rename_failed(src_key, dst_key);
                    // Fall through to copy-then-delete with the ALREADY resolved
                    // destination. Nothing is there: an Overwrite unlinked it
                    // above, KeepBoth picked a free name, and rename_no_replace
                    // would have refused had anything appeared since.
                    return self.copy_file_inner(src, &planned, src_meta, true, false);
                }
                Err(err) => {
                    self.queue_error(src, Stage::Rename, err);
                    return self.continue_or_fatal();
                }
            }
        }

        let (planned, replacing) = match self.resolve_dest(src, dest) {
            DestPlan::Proceed { path, replacing } => (path, replacing),
            DestPlan::Skip => {
                self.files_skipped += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                return Step::Ok;
            }
            DestPlan::Cancel => return Step::Cancelled,
        };
        self.copy_file_inner(
            src,
            &planned,
            src_meta,
            self.spec.op == Operation::Move,
            replacing,
        )
    }

    /// The copy path shared by Copy jobs and cross-fs Move fallback.
    /// `delete_source` only ever happens after verify + rename.
    fn copy_file_inner(
        &mut self,
        src: &Path,
        dest: &Path,
        src_meta: &std::fs::Metadata,
        delete_source: bool,
        // The destination is occupied and the user chose to replace it.
        // Nothing has been done about that yet: the old file is moved out of
        // the way below, once this copy has something to put in its place.
        replacing: bool,
    ) -> Step {
        // Hardlink preservation: second and later links to an inode we already
        // copied become hardlinks to the first copy.
        if src_meta.nlink() > 1 {
            let key = (src_meta.dev(), src_meta.ino());
            if let Some(first) = self.hardlinks.get(&key).cloned() {
                // On failure (cross-device, fs without hardlinks) fall through
                // to a full copy instead.
                if rustix::fs::linkat(
                    rustix::fs::CWD,
                    &first,
                    rustix::fs::CWD,
                    dest,
                    rustix::fs::AtFlags::empty(),
                )
                .is_ok()
                {
                    self.stats.hardlinks += 1;
                    // A link to something this job already wrote. Undoing it
                    // is removing the name, not the inode, so it needs no
                    // identity of its own beyond where it points.
                    if let Ok(meta) = std::fs::symlink_metadata(dest) {
                        self.undone(Undone::Created {
                            path: dest.to_path_buf(),
                            from: delete_source.then(|| src.to_path_buf()),
                            dev: meta.dev(),
                            ino: meta.ino(),
                            len: meta.len(),
                            mtime: meta.modified().ok(),
                        });
                    }
                    self.files_copied += 1;
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    // Bytes for this file were already counted in totals;
                    // count them done so the bar completes.
                    self.progress
                        .bytes_done
                        .fetch_add(src_meta.len(), Ordering::Relaxed);
                    if delete_source && let Err(err) = std::fs::remove_file(src) {
                        self.queue_error(src, Stage::DeleteSource, err);
                    }
                    return Step::Ok;
                }
            } else {
                self.hardlinks.insert(key, dest.to_path_buf());
            }
        }

        let src_file = match File::open(src) {
            Ok(f) => f,
            Err(err) => {
                self.queue_error(src, Stage::Open, err);
                return self.continue_or_fatal();
            }
        };

        let tmp_path = sys::partial_path(dest);
        let tmp = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
        {
            Ok(f) => f,
            Err(err) => {
                self.queue_error(dest, Stage::Open, err);
                return self.continue_or_fatal();
            }
        };

        let result = self.copy_into_tmp(&src_file, &tmp, src, src_meta);

        match result {
            Ok(CopyOutcome::Cancelled) => {
                drop(tmp);
                let _ = std::fs::remove_file(&tmp_path);
                Step::Cancelled
            }
            Ok(CopyOutcome::Done(mechanism)) => {
                match mechanism {
                    CopyMechanism::Reflink => self.stats.reflinks += 1,
                    CopyMechanism::CopyFileRange => self.stats.copy_file_range += 1,
                    CopyMechanism::ReadWrite => self.stats.read_write += 1,
                }

                let meta_outcome = meta::apply_file_meta(&src_file, src_meta, &tmp);
                for w in meta_outcome.warnings {
                    self.warn(dest, w);
                }

                // Atomic replace over an existing file needs the data durable
                // before the rename; fresh destinations keep cp-parity speed.
                // Keyed on what the user chose, not on whether the trash took
                // the old file: a filesystem with no trash is exactly the
                // removable kind most likely to be pulled mid-writeback, and
                // skipping the fsync there was the opposite of the intent.
                if replacing && let Err(err) = rustix::fs::fsync(&tmp) {
                    drop(tmp);
                    let _ = std::fs::remove_file(&tmp_path);
                    self.queue_error(dest, Stage::Write, err.into());
                    return self.continue_or_fatal();
                }
                // While the descriptor is still open, so this is an fstat
                // rather than a path lookup, and after `apply_file_meta`, so
                // the mtime is the one the file will actually carry.
                let landed = tmp.metadata().ok();
                drop(tmp);

                // The last moment before anything is destroyed, and the whole
                // reason this is here rather than at resolve time: everything
                // that could still fail — opening the source, reading it,
                // running out of space, the metadata, the fsync — has already
                // happened. Displacing at resolve time meant a copy that then
                // failed to open its source had already emptied the
                // destination, leaving the user's file only in the trash and
                // the job reporting an error about something else entirely.
                if replacing {
                    self.displace(dest);
                }

                if let Err(err) = std::fs::rename(&tmp_path, dest) {
                    let _ = std::fs::remove_file(&tmp_path);
                    self.queue_error(dest, Stage::Rename, err);
                    return self.continue_or_fatal();
                }

                // Whatever the rename replaced is in the trash and already
                // recorded, so this is simply a file that was not here before.
                if let Some(meta) = landed {
                    self.undone(Undone::Created {
                        path: dest.to_path_buf(),
                        from: delete_source.then(|| src.to_path_buf()),
                        dev: meta.dev(),
                        ino: meta.ino(),
                        len: meta.len(),
                        mtime: meta.modified().ok(),
                    });
                }

                self.files_copied += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);

                if delete_source && let Err(err) = std::fs::remove_file(src) {
                    self.queue_error(src, Stage::DeleteSource, err);
                }
                Step::Ok
            }
            Err(err) => {
                drop(tmp);
                let _ = std::fs::remove_file(&tmp_path);
                self.queue_error(src, Stage::Write, err);
                self.continue_or_fatal()
            }
        }
    }

    fn copy_into_tmp(
        &mut self,
        src_file: &File,
        tmp: &File,
        src: &Path,
        src_meta: &std::fs::Metadata,
    ) -> std::io::Result<CopyOutcome> {
        // Tier 1: reflink, unless this mount pair already refused.
        if let (Some(src_key), Some(dst_key)) = (self.src_mount_key(src, src_meta), self.dest_mount)
            && self.caps.reflink_worth_trying(src_key, dst_key)
        {
            match copy::try_reflink(src_file, tmp) {
                Ok(()) => {
                    self.progress
                        .bytes_done
                        .fetch_add(src_meta.len(), Ordering::Relaxed);
                    return Ok(CopyOutcome::Done(CopyMechanism::Reflink));
                }
                // Any refusal (EXDEV, EOPNOTSUPP, EINVAL, EBADF…) caches; a
                // mount pair cannot start supporting FICLONE mid-job.
                Err(_) => self.caps.mark_reflink_failed(src_key, dst_key),
            }
        }

        // Tier 2.
        copy::copy_contents(
            src_file,
            tmp,
            src_meta.len(),
            &self.progress.bytes_done,
            &self.cancel,
            &mut self.copy_buf,
            // st_blocks is in 512-byte units. Short of the length means holes.
            (src_meta.blocks() * 512) < src_meta.len(),
        )
    }

    fn resolve_dest(&mut self, src: &Path, dest: &Path) -> DestPlan {
        // lstat: a dangling symlink at dest is still a conflict.
        if std::fs::symlink_metadata(dest).is_err() {
            return DestPlan::Proceed {
                path: dest.to_path_buf(),
                replacing: false,
            };
        }
        self.progress.set_phase(Phase::AwaitingConflict);
        let resolution = self.conflicts.resolve(src, dest);
        self.progress.set_phase(Phase::Transferring);
        match resolution {
            Resolution::CancelJob => DestPlan::Cancel,
            Resolution::Proceed(ConflictChoice::Skip) => DestPlan::Skip,
            Resolution::Proceed(ConflictChoice::Overwrite) => DestPlan::Proceed {
                path: dest.to_path_buf(),
                replacing: true,
            },
            Resolution::Proceed(ConflictChoice::KeepBoth) => {
                for attempt in 1..1000 {
                    let candidate = sys::keep_both_name(dest, attempt);
                    if std::fs::symlink_metadata(&candidate).is_err() {
                        // Chosen because nothing is there.
                        return DestPlan::Proceed {
                            path: candidate,
                            replacing: false,
                        };
                    }
                }
                DestPlan::Skip
            }
        }
    }

    /// Move whatever is at `dest` into the trash, so replacing it is
    /// reversible.
    ///
    /// Falls back to leaving it in place — the caller then overwrites it as
    /// before — when this filesystem has nowhere to put it. `trash` only ever
    /// renames and refuses rather than copying across a boundary, which is the
    /// behaviour that keeps a delete instant, and the same refusal here is a
    /// FAT stick or a mount owned by root. Failing the paste instead would be
    /// a regression nobody asked for, so it warns once and carries on.
    fn displace(&mut self, dest: &Path) -> Option<TrashedItem> {
        let device = match std::fs::symlink_metadata(dest) {
            Ok(meta) => meta.dev(),
            Err(_) => return None,
        };
        let resolved = self
            .trash_dirs
            .entry(device)
            .or_insert_with(|| TrashDir::for_path(dest).ok())
            .is_some();
        if !resolved {
            if !self.warned_no_trash {
                self.warned_no_trash = true;
                self.warn(
                    dest,
                    "nothing here can hold a trash, so replaced files cannot be restored"
                        .to_string(),
                );
            }
            self.undone(Undone::Lost(dest.to_path_buf()));
            return None;
        }
        // Names repeat: a sync of one tree over another overwrites a thousand
        // files called `index.html`, and each would otherwise rescan the whole
        // run of `.2`, `.3`, … from the start. Remembering where the last one
        // landed makes that linear overall instead of quadratic.
        let name = dest.file_name()?.to_string_lossy().into_owned();
        let start = self.trash_names.get(&name).copied().unwrap_or(1);
        // Scoped so the loan on `self` ends before the records below.
        let put = {
            let bin = self.trash_dirs.get(&device).and_then(Option::as_ref)?;
            bin.put(dest, start)
        };
        match put {
            Ok((item, attempt)) => {
                self.trash_names.insert(name, attempt);
                self.undone(Undone::Displaced(item.clone()));
                Some(item)
            }
            Err(err) => {
                self.warn(dest, format!("could not be moved to the trash: {err}"));
                self.undone(Undone::Lost(dest.to_path_buf()));
                None
            }
        }
    }
}

enum DestPlan {
    /// `replacing` answers what a second `dest.exists()` used to ask:
    /// resolving a destination already stats it, so asking again was a syscall
    /// per file for something we had just learned.
    ///
    /// It says the destination is occupied and the user chose to replace it —
    /// not that anything has been done about that yet. Moving the old file out
    /// of the way is each site's own last act before its destructive one,
    /// because doing it here, at resolve time, meant a copy that then failed
    /// to even open its source had already emptied the destination.
    Proceed {
        path: PathBuf,
        replacing: bool,
    },
    Skip,
    Cancel,
}

use std::os::unix::fs::OpenOptionsExt;
