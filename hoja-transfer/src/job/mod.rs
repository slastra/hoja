//! Job lifecycle: spec validation, the worker thread, the walk-while-copying
//! loop, and the per-file tier ladder.

use std::collections::{HashMap, HashSet};
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

mod undo;

/// How many changes a job may record before it stops offering to undo itself.
///
/// Reached only by a copy that merged into an existing tree, since anything
/// landing on a fresh name collapses to one record. Sized like
/// `MAX_RETAINED_FAILURES` in the UI: high enough that no ordinary transfer
/// approaches it, low enough that the log cannot become the reason a machine
/// runs out of memory.
const MAX_UNDO_RECORDS: usize = 100_000;

/// How long moved sources may wait for a sync, and how many bytes of copies
/// they may stand for, before one is forced. See `Worker::release_source`.
///
/// The bound is on what a failure can cost: a stick pulled mid-batch loses
/// the batch's copies, while their sources are still in place. Time and bytes
/// measure that; a count of files does not. One sync per thousand files cost
/// 0.3 s on a move of 20,000 small files to an NVMe disk, half as much again
/// as the move. By time, that move syncs once, at the end, and takes as long
/// as the old one did followed by a `sync`. The count is only a ceiling on the
/// memory the queue holds.
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const FLUSH_EVERY_BYTES: u64 = 256 * 1024 * 1024;
const FLUSH_MAX_WAITING: usize = 50_000;

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub op: Operation,
    /// Top-level items (files or directories); each lands at `dest_dir/<file_name>`.
    pub sources: Vec<PathBuf>,
    /// Must be an existing directory.
    pub dest_dir: PathBuf,
    pub policy: JobPolicy,
    /// A directory the caller filled on this job's behalf, whose contents are
    /// the sources and are nobody's once the job is done.
    ///
    /// Copying out of an archive works this way: the members are extracted
    /// into a scratch directory and then moved into place, and the caller
    /// removes the scratch directory afterwards. Undo therefore cannot put
    /// them back where they came from — where they came from was deleted, and
    /// before that it was a temporary directory nobody wants restored. What it
    /// records instead is that the files are new, so taking them away is the
    /// whole of the inverse. The archive still has them.
    pub staging: Option<PathBuf>,
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
    if let Some(source) = dest_inside_source(&spec.dest_dir, &spec.sources) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "destination {} is inside source {}",
                spec.dest_dir.display(),
                source.display()
            ),
        ));
    }

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
    start_worker(label, "hoja-transfer", spec, Worker::run)
}

/// The source that `dest_dir` lies inside, if there is one.
///
/// By the real path as well as by the path as written. A pane that reached a
/// folder through a symlink names it by the link, so `dest_dir.starts_with`
/// alone let a folder be copied into its own subfolder. The walk then found its
/// own output, recursed into it without end and overflowed the worker's stack,
/// which aborts the whole process.
///
/// Only a source that is a real directory is resolved. A symlink is copied as
/// a link and never walked, so it cannot recurse, and a file has no inside.
/// That keeps this to one lstat for each file pasted, on the caller's thread.
fn dest_inside_source<'a>(dest_dir: &Path, sources: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let real_dest = std::fs::canonicalize(dest_dir).ok();
    sources.iter().find(|source| {
        if dest_dir.starts_with(source) {
            return true;
        }
        let Some(real_dest) = &real_dest else {
            return false;
        };
        std::fs::symlink_metadata(source).is_ok_and(|meta| meta.is_dir())
            && std::fs::canonicalize(source).is_ok_and(|real| real_dest.starts_with(real))
    })
}

/// Spawn the thread a job runs on, and hand back the handle that steers it.
fn start_worker(
    label: String,
    thread_name: &str,
    spec: JobSpec,
    body: impl FnOnce(&mut Worker) + Send + 'static,
) -> std::io::Result<JobHandle> {
    let id = JobId(NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let (events_tx, events_rx) = mpsc::channel();

    let thread = std::thread::Builder::new()
        .name(format!("{thread_name}-{}", id.0))
        .spawn({
            let progress = progress.clone();
            let cancel = cancel.clone();
            let pause = pause.clone();
            move || body(&mut Worker::new(spec, progress, events_tx, cancel, pause))
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
        staging: None,
    };
    start_worker(
        format!("Undoing {label}"),
        "hoja-undo",
        spec,
        move |worker| worker.run_undo(records),
    )
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
    /// (dev, ino) of each directory this job created.
    ///
    /// So a walk never descends into its own output. `spawn_job` refuses a
    /// destination inside a source by path, but a bind mount can put one
    /// there under a name no path comparison will see, and walking it copies
    /// the copy until the stack runs out, which aborts the whole process.
    made_dirs: HashSet<(u64, u64)>,
    /// Sources of a cross-filesystem move whose copies have landed, waiting
    /// for a sync before they are removed. In the order they were released,
    /// which puts a directory after its own files.
    pending: Vec<Release>,
    pending_bytes: u64,
    /// The destination folder `release_source` last noted the device of.
    pending_parent: Option<PathBuf>,
    /// When the oldest waiting source was released.
    pending_since: Option<std::time::Instant>,
    /// One directory on each filesystem the waiting copies are on, by device:
    /// what `syncfs` is called through.
    pending_devs: HashMap<u64, PathBuf>,
    /// Undo only: records whose directory was queued in `pending` and then
    /// could not be removed, so they are still to be reversed.
    owed: Vec<Undone>,
}

/// A move's source, waiting to be removed. See `Worker::release_source`.
enum Release {
    File(PathBuf),
    /// Removed only if empty, so whatever stayed in it keeps it.
    Dir {
        path: PathBuf,
        /// Noted for undo once it is gone.
        done: Option<Undone>,
        /// Owed back to undo if it cannot go.
        owed: Option<Undone>,
    },
}

/// What Tier 0 made of a file or symlink.
enum Placed {
    /// Renamed into place, skipped, or failed with the error queued.
    Done(Step),
    /// Not renamed: copy it to `planned`, which is settled and, if
    /// `replacing`, still occupied by what the copy will replace.
    Copy { planned: PathBuf, replacing: bool },
}

/// Where a finished temp file ended up.
enum Landed {
    At(PathBuf),
    /// A conflict found at the last moment was answered with Skip.
    Skipped,
    Cancelled,
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
            made_dirs: HashSet::new(),
            pending: Vec::new(),
            pending_bytes: 0,
            pending_since: None,
            pending_parent: None,
            pending_devs: HashMap::new(),
            owed: Vec::new(),
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
                let Some(dest) = self.place_beside_itself(&src, self.spec.dest_dir.join(name))
                else {
                    // A move to where it already is.
                    self.skipped();
                    continue;
                };
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

        // Whatever a move left waiting goes now, on every way out: a cancelled
        // move still removes the sources of what it finished, so cancelling
        // never leaves a file in both places.
        if !self.pending.is_empty() {
            self.progress.set_phase(Phase::Flushing);
            self.flush_sources();
        }

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

    /// Whether this source was put there by the caller for this job alone, in
    /// which case there is nowhere to move it back to.
    fn was_staged(&self, src: &Path) -> bool {
        self.spec
            .staging
            .as_ref()
            .is_some_and(|staging| src.starts_with(staging))
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
    /// and a directory this job made is never walked as a source (see
    /// `made_dirs`), so recursion is safe here.
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
            return self.transfer_symlink(src, dest, &src_meta, counted);
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

    /// Where a top-level source should land when `dest` is the source itself.
    ///
    /// Pasting into the folder something was cut or copied from names the same
    /// file twice, and the conflict that follows is not one. Answering it with
    /// Overwrite moved the "existing" file out of the way, and that file was
    /// the source: a move then had nothing left to rename, so the file went to
    /// the trash, or was unlinked outright where there is no trash. A folder
    /// lost every file in it the same way.
    ///
    /// So: a move to where a thing already is has nothing to do (`None`), and a
    /// copy beside itself takes the first free " (copy)" name without asking,
    /// which is what every file manager does on paste-in-place.
    ///
    /// Checked once per top-level source, not per file. Below the top, a source
    /// and its destination are the same only if the top ones were, and an
    /// extra stat for every file would be paid on every transfer.
    fn place_beside_itself(&mut self, src: &Path, dest: PathBuf) -> Option<PathBuf> {
        let (Ok(src_meta), Ok(dest_meta)) = (
            std::fs::symlink_metadata(src),
            std::fs::symlink_metadata(&dest),
        ) else {
            return Some(dest);
        };
        if (src_meta.dev(), src_meta.ino()) != (dest_meta.dev(), dest_meta.ino()) {
            return Some(dest);
        }
        match self.spec.op {
            Operation::Move => None,
            Operation::Copy => (1..1000)
                .map(|attempt| sys::keep_both_name(&dest, attempt))
                .find(|candidate| std::fs::symlink_metadata(candidate).is_err()),
        }
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

    /// Count an entry the job decided not to touch.
    fn skipped(&mut self) -> Step {
        self.files_skipped += 1;
        self.progress.files_done.fetch_add(1, Ordering::Relaxed);
        Step::Ok
    }

    /// Whether a record made now would be kept, so a stat taken only to build
    /// one can be skipped when it would not. Inside a directory this job made,
    /// which is where nearly every file of a large copy lands, it would not.
    fn recording(&self) -> bool {
        self.undoable && !self.claimed
    }

    /// Note that `path` is new, for undo to take away again.
    ///
    /// `landed` is its metadata if the caller already holds it; otherwise it is
    /// looked up here, and only if the record would be kept. `from` is where it
    /// came from when the source is gone (a move), so undo can put it back.
    fn record_created(
        &mut self,
        path: &Path,
        landed: Option<std::fs::Metadata>,
        from: Option<&Path>,
    ) {
        if !self.recording() {
            return;
        }
        let Some(meta) = landed.or_else(|| std::fs::symlink_metadata(path).ok()) else {
            return;
        };
        self.undone(Undone::Created {
            path: path.to_path_buf(),
            from: from.map(Path::to_path_buf),
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.modified().ok(),
        });
    }

    /// Where undo should send a moved file back to, if anywhere.
    fn origin<'a>(&self, src: &'a Path, delete_source: bool) -> Option<&'a Path> {
        (delete_source && !self.was_staged(src)).then_some(src)
    }

    /// Clear the name `path` for a replacement the user asked for: into the
    /// trash where there is one, unlinked where there is not.
    ///
    /// For the two writes that cannot replace on their own, `symlink()` and a
    /// NOREPLACE rename. The unlink runs either way, which is what actually
    /// clears the name when the trash refused; it fails harmlessly on a
    /// directory, and the write after it then reports the conflict.
    fn clear_for_replace(&mut self, path: &Path) {
        self.displace(path);
        let _ = std::fs::remove_file(path);
    }

    /// Tier 0 for one file or symlink: settle the destination, then try to
    /// rename the source onto it.
    ///
    /// The rename is tried *before* anything at the destination is touched.
    /// Clearing the name first meant that a move across filesystems, where the
    /// rename fails EXDEV, had already put the file it was replacing in the
    /// trash (or unlinked it, with no trash) before a copy that could still
    /// fail had even opened its source. Now EXDEV hands the copy a destination
    /// that is still whole, and the copy displaces it only once its own bytes
    /// are written. Only an AlreadyExists clears the name, and then only for a
    /// replacement the user asked for.
    fn rename_into_place(
        &mut self,
        src: &Path,
        dest: &Path,
        src_meta: &std::fs::Metadata,
    ) -> Placed {
        let (mut planned, mut replacing) = match self.resolve_dest(src, dest) {
            DestPlan::Proceed { path, replacing } => (path, replacing),
            DestPlan::Skip => return Placed::Done(self.skipped()),
            DestPlan::Cancel => return Placed::Done(Step::Cancelled),
        };
        let (Operation::Move, Some(src_key), Some(dst_key)) = (
            self.spec.op,
            self.src_mount_key(src, src_meta),
            self.dest_mount,
        ) else {
            return Placed::Copy { planned, replacing };
        };
        if !self.caps.rename_worth_trying(src_key, dst_key) {
            return Placed::Copy { planned, replacing };
        }

        let mut retried = false;
        loop {
            match sys::rename_no_replace(src, &planned) {
                Ok(()) => break,
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                    self.caps.mark_rename_failed(src_key, dst_key);
                    return Placed::Copy { planned, replacing };
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && !retried => {
                    retried = true;
                    if !replacing {
                        // Something appeared since the destination was
                        // settled: ask again about what is there now.
                        match self.resolve_dest(src, &planned) {
                            DestPlan::Proceed {
                                path,
                                replacing: again,
                            } => {
                                planned = path;
                                replacing = again;
                            }
                            DestPlan::Skip => return Placed::Done(self.skipped()),
                            DestPlan::Cancel => return Placed::Done(Step::Cancelled),
                        }
                    }
                    if replacing {
                        self.clear_for_replace(&planned);
                    }
                }
                Err(err) => {
                    self.queue_error(src, Stage::Rename, err);
                    return Placed::Done(self.continue_or_fatal());
                }
            }
        }

        self.stats.renames += 1;
        if self.was_staged(src) {
            // See `JobSpec::staging`: nowhere to put it back, so what undo
            // owes is to take it away.
            self.record_created(&planned, None, None);
        } else {
            self.undone(Undone::Renamed {
                from: src.to_path_buf(),
                to: planned.clone(),
                dev: src_meta.dev(),
                ino: src_meta.ino(),
            });
        }
        self.files_copied += 1;
        self.progress.files_done.fetch_add(1, Ordering::Relaxed);
        if src_meta.is_file() {
            self.progress
                .bytes_done
                .fetch_add(src_meta.len(), Ordering::Relaxed);
        }
        Placed::Done(Step::Ok)
    }

    fn transfer_symlink(
        &mut self,
        src: &Path,
        dest: &Path,
        src_meta: &std::fs::Metadata,
        counted: bool,
    ) -> Step {
        if !counted && !self.scanned {
            self.progress.files_total.fetch_add(1, Ordering::Relaxed);
        }
        // A move on one filesystem renames the link like any file, which keeps
        // its own mtime and owner. Recreating it is for crossing a boundary.
        let (dest, replacing) = match self.rename_into_place(src, dest, src_meta) {
            Placed::Done(step) => return step,
            Placed::Copy { planned, replacing } => (planned, replacing),
        };
        // `symlink()` has no replace semantics, so the name has to be clear.
        // Nothing is written before this point, so this is the last moment.
        if replacing {
            self.clear_for_replace(&dest);
        }
        match meta::copy_symlink(src, &dest) {
            Ok(()) => {
                self.stats.symlinks += 1;
                let delete_source = self.spec.op == Operation::Move;
                let from = self.origin(src, delete_source);
                self.record_created(&dest, None, from);
                self.files_copied += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                if delete_source {
                    self.release_source(src, &dest, 0);
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
                    if self.was_staged(src) {
                        // Nowhere to put it back: this came out of a scratch
                        // directory the caller is about to remove. Undoing it
                        // means taking it away, which is what a directory this
                        // job made records.
                        self.undone(Undone::CreatedDir {
                            path: dest.to_path_buf(),
                            whole: true,
                            mtime: None,
                        });
                    } else {
                        self.undone(Undone::Renamed {
                            from: src.to_path_buf(),
                            to: dest.to_path_buf(),
                            dev: src_meta.dev(),
                            ino: src_meta.ino(),
                        });
                    }
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
        if made_it {
            if let Err(err) = std::fs::create_dir(dest) {
                self.queue_error(dest, Stage::CreateDir, err);
                return self.continue_or_fatal();
            }
            // So the walk can tell its own output from its input. See
            // `made_dirs`.
            if let Ok(meta) = std::fs::symlink_metadata(dest) {
                self.made_dirs.insert((meta.dev(), meta.ino()));
            }
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
        let step = self.process_dir_children(src, dest, src_meta, made_it);
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
        made_it: bool,
    ) -> Step {
        use std::os::unix::fs::DirEntryExt;

        // Enumerate children first so totals grow ahead of processing (this IS
        // the one walk: gate counters for M3 accumulate on these same adds).
        let entries = match std::fs::read_dir(src) {
            Ok(iter) => {
                let mut v: Vec<_> = iter
                    .filter_map(Result::ok)
                    // A directory this job made, met again as a source: the
                    // destination is inside the source by some route the
                    // check in `spawn_job` could not see, such as a bind
                    // mount. Walking it would copy the copy, without end.
                    // `d_ino` comes with the entry, so this costs no syscall.
                    .filter(|e| !self.made_dirs.contains(&(src_meta.dev(), e.ino())))
                    .collect();
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
                // Counted the way `scan` counts: a symlink is one file and no
                // bytes. Leaving symlinks out here while `files_done` counted
                // them let the bar run past the end.
                if let Ok(m) = entry.metadata() {
                    if m.is_file() {
                        self.progress.files_total.fetch_add(1, Ordering::Relaxed);
                        self.progress
                            .bytes_total
                            .fetch_add(m.len(), Ordering::Relaxed);
                    } else if m.file_type().is_symlink() {
                        self.progress.files_total.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // Whether anything below stays behind, from counters rather than from
        // the error list. Searching every queued error for each child was a
        // scan of the whole list per entry, quadratic exactly when a transfer
        // fails a lot, and it missed a skipped conflict entirely: the move
        // then tried to remove a folder that still held the skipped file and
        // reported "directory not empty" for a choice the user made.
        let before = (self.errors.len(), self.files_skipped);
        for entry in entries {
            let child_src = entry.path();
            let child_dest = dest.join(entry.file_name());
            match self.process_item(&child_src, &child_dest, true) {
                Step::Ok => {}
                Step::Cancelled => return Step::Cancelled,
                Step::Fatal => return Step::Fatal,
            }
        }
        let left_behind = (self.errors.len(), self.files_skipped) != before;

        // Metadata after children: writes inside would clobber the times. Only
        // on a directory this job made: merging into one that was already
        // there gave it the source's mode, so copying a private folder into a
        // shared one of the same name made the shared one private.
        if made_it {
            let outcome = meta::apply_dir_meta(src_meta, dest);
            for w in outcome.warnings {
                self.warn(dest, w);
            }
        }

        // Move: remove the now-empty source dir, but never when something was
        // left in it, that would orphan whatever is still inside.
        if self.spec.op == Operation::Move && !left_behind {
            self.release_dir(src, src_meta);
        }
        Step::Ok
    }

    fn transfer_file(&mut self, src: &Path, dest: &Path, src_meta: &std::fs::Metadata) -> Step {
        self.progress.set_current(src);
        match self.rename_into_place(src, dest, src_meta) {
            Placed::Done(step) => step,
            Placed::Copy { planned, replacing } => self.copy_file_inner(
                src,
                &planned,
                src_meta,
                self.spec.op == Operation::Move,
                replacing,
            ),
        }
    }

    /// The copy path shared by Copy jobs and cross-fs Move fallback.
    /// `delete_source` only ever happens after the copy has landed, and then
    /// only once a sync has confirmed it (see `release_source`).
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
                // On failure (cross-device, fs without hardlinks, or an
                // occupied name) fall through to a full copy instead.
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
                    let from = self.origin(src, delete_source);
                    self.record_created(dest, None, from);
                    self.files_copied += 1;
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    // Bytes for this file were already counted in totals;
                    // count them done so the bar completes.
                    self.progress
                        .bytes_done
                        .fetch_add(src_meta.len(), Ordering::Relaxed);
                    if delete_source {
                        self.release_source(src, dest, 0);
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

        let mechanism = match self.copy_into_tmp(&src_file, &tmp, src, src_meta) {
            Ok(CopyOutcome::Done(mechanism)) => mechanism,
            Ok(CopyOutcome::Cancelled) => {
                drop(tmp);
                let _ = std::fs::remove_file(&tmp_path);
                return Step::Cancelled;
            }
            Err(err) => {
                drop(tmp);
                let _ = std::fs::remove_file(&tmp_path);
                self.queue_error(src, Stage::Write, err);
                return self.continue_or_fatal();
            }
        };
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
        // While the descriptor is still open, so this is an fstat rather than
        // a path lookup, and after `apply_file_meta`, so the mtime is the one
        // the file will actually carry. Only when a record will be kept:
        // inside a directory this job made, which is most files of a large
        // copy, it would not be.
        let landed = if self.recording() {
            tmp.metadata().ok()
        } else {
            None
        };
        drop(tmp);

        let Some(dest) = self.land(src, &tmp_path, dest, replacing) else {
            return self.continue_or_fatal();
        };
        let dest = match dest {
            Landed::At(path) => path,
            Landed::Skipped => return self.skipped(),
            Landed::Cancelled => return Step::Cancelled,
        };

        // Whatever the rename replaced is in the trash and already
        // recorded, so this is simply a file that was not here before.
        let from = self.origin(src, delete_source);
        self.record_created(&dest, landed, from);

        self.files_copied += 1;
        self.progress.files_done.fetch_add(1, Ordering::Relaxed);

        if delete_source {
            self.release_source(src, &dest, src_meta.len());
        }
        Step::Ok
    }

    /// Rename a finished temp file onto its destination. `None` means it
    /// failed and the error is queued; the temp file is gone either way.
    ///
    /// A replacement displaces what is there and renames over the name, the
    /// last moment before anything is destroyed. Everything that could still
    /// fail (opening the source, reading it, running out of space, the
    /// metadata, the fsync) has already happened. Displacing at resolve time
    /// meant a copy that then failed to open its source had already emptied
    /// the destination.
    ///
    /// A fresh destination is renamed onto with NOREPLACE. A plain rename
    /// there replaced, without asking, whatever another program had put at
    /// that name while a long copy was running; now that is a conflict like
    /// any other, asked about when it is found.
    fn land(
        &mut self,
        src: &Path,
        tmp_path: &Path,
        dest: &Path,
        replacing: bool,
    ) -> Option<Landed> {
        let mut dest = dest.to_path_buf();
        let mut replacing = replacing;
        loop {
            let renamed = if replacing {
                self.displace(&dest);
                std::fs::rename(tmp_path, &dest)
            } else {
                sys::rename_no_replace(tmp_path, &dest)
            };
            match renamed {
                Ok(()) => return Some(Landed::At(dest)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && !replacing => {
                    // Somebody else's file arrived while this one copied.
                    match self.resolve_dest(src, &dest) {
                        DestPlan::Proceed {
                            path,
                            replacing: again,
                        } => {
                            dest = path;
                            replacing = again;
                        }
                        DestPlan::Skip => {
                            let _ = std::fs::remove_file(tmp_path);
                            return Some(Landed::Skipped);
                        }
                        DestPlan::Cancel => {
                            let _ = std::fs::remove_file(tmp_path);
                            return Some(Landed::Cancelled);
                        }
                    }
                }
                Err(err) => {
                    let _ = std::fs::remove_file(tmp_path);
                    self.queue_error(&dest, Stage::Rename, err);
                    return None;
                }
            }
        }
    }

    /// Remove a move's source, once its copy at `dest` is known to be on disk.
    ///
    /// Not at once. A copy that has landed is in the page cache, not
    /// necessarily on the device, and unlinking the source straight after the
    /// rename left a window where the only copy was dirty pages: pull a USB
    /// stick, or have its writeback fail, and the files already "moved" were
    /// gone from both ends. The single `syncfs` at the end did not close it,
    /// since by then every source was already deleted, and a sync that failed
    /// reported an error about files that no longer existed anywhere.
    ///
    /// So the source waits in `pending` until a `syncfs` of the filesystem its
    /// copy is on has succeeded. See `flush_sources`.
    fn release_source(&mut self, src: &Path, dest: &Path, len: u64) {
        // One stat per destination folder rather than per file: files arrive
        // a folder at a time, so the last one asked about is nearly always it.
        if let Some(parent) = dest.parent()
            && self.pending_parent.as_deref() != Some(parent)
        {
            if let Ok(meta) = std::fs::symlink_metadata(parent) {
                self.pending_devs
                    .entry(meta.dev())
                    .or_insert_with(|| parent.to_path_buf());
            }
            self.pending_parent = Some(parent.to_path_buf());
        }
        self.pending.push(Release::File(src.to_path_buf()));
        self.pending_bytes += len;
        let since = *self
            .pending_since
            .get_or_insert_with(std::time::Instant::now);
        if self.pending_bytes >= FLUSH_EVERY_BYTES
            || self.pending.len() >= FLUSH_MAX_WAITING
            || since.elapsed() >= FLUSH_EVERY
        {
            self.flush_sources();
        }
    }

    /// Remove a move's emptied source directory.
    ///
    /// After any of its files still waiting in `pending`, so it joins the queue
    /// when there is one. With nothing waiting, its files are already gone and
    /// it can go now.
    fn release_dir(&mut self, src: &Path, src_meta: &std::fs::Metadata) {
        // Recorded after the children, so replaying backwards makes the
        // directory again before trying to move anything into it. Without
        // this every child's `Renamed` failed ENOENT on a parent that no
        // longer existed, and a merged move could not be undone at all.
        let done = Some(Undone::RemovedDir {
            path: src.to_path_buf(),
            mode: Some(src_meta.mode()),
            mtime: Some((src_meta.mtime(), src_meta.mtime_nsec())),
        });
        if self.pending.is_empty() {
            self.remove_source_dir(src, done, None);
        } else {
            self.pending.push(Release::Dir {
                path: src.to_path_buf(),
                done,
                owed: None,
            });
        }
    }

    fn remove_source_dir(&mut self, path: &Path, done: Option<Undone>, owed: Option<Undone>) {
        match std::fs::remove_dir(path) {
            Ok(()) => {
                if let Some(record) = done {
                    self.undone(record);
                }
            }
            Err(err) => {
                self.queue_error(path, Stage::DeleteSource, err);
                self.owed.extend(owed);
            }
        }
    }

    /// Sync every filesystem a waiting source's copy is on, then delete the
    /// sources. A sync that fails keeps them all: a source is only removed
    /// once its copy is known to be safe.
    fn flush_sources(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut synced = true;
        for (_, dir) in std::mem::take(&mut self.pending_devs) {
            let result =
                File::open(&dir).and_then(|fd| rustix::fs::syncfs(&fd).map_err(Into::into));
            if let Err(err) = result {
                self.queue_error(&dir, Stage::Sync, err);
                synced = false;
            }
        }
        for release in std::mem::take(&mut self.pending) {
            match release {
                Release::File(path) if synced => {
                    if let Err(err) = std::fs::remove_file(&path) {
                        self.queue_error(&path, Stage::DeleteSource, err);
                    }
                }
                Release::File(path) => self.queue_error(
                    &path,
                    Stage::Sync,
                    std::io::Error::other("the copy was not confirmed on disk, so this was kept"),
                ),
                Release::Dir { path, done, owed } if synced => {
                    self.remove_source_dir(&path, done, owed)
                }
                // Still holds the files kept above. Each has its own error.
                Release::Dir { owed, .. } => self.owed.extend(owed),
            }
        }
        self.pending_bytes = 0;
        self.pending_since = None;
        // `pending_devs` was emptied by the sync, so the next folder has to be
        // noted again even if it is this one.
        self.pending_parent = None;
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

    /// Move `path` into the trash for whatever filesystem it is on.
    ///
    /// `None` when nothing on that filesystem can hold one. The device map and
    /// the name-counter hint live here rather than at each call site: the
    /// resolution is per filesystem because a job's destination tree can span
    /// a mount point, and the counter is what keeps a thousand files called
    /// `index.html` from rescanning the whole run of `.2`, `.3`, … each time.
    ///
    /// What to *record* about it is the caller's, and the two differ: a
    /// transfer notes where the old file went, an undo only needs it gone.
    fn bin(&mut self, path: &Path) -> Option<std::io::Result<TrashedItem>> {
        let device = std::fs::symlink_metadata(path).ok()?.dev();
        self.trash_dirs
            .entry(device)
            .or_insert_with(|| TrashDir::for_path(path).ok());
        let name = path.file_name()?.to_string_lossy().into_owned();
        let start = self.trash_names.get(&name).copied().unwrap_or(1);
        // Scoped so the loan on `self` ends before the caller's records.
        let put = self
            .trash_dirs
            .get(&device)
            .and_then(Option::as_ref)?
            .put(path, start);
        Some(match put {
            Ok((item, attempt)) => {
                self.trash_names.insert(name, attempt);
                Ok(item)
            }
            Err(err) => Err(err),
        })
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
        // Gone already, so there is nothing to keep. Without this, `bin`
        // answered `None` for the missing file and it was recorded as lost,
        // under a warning that this filesystem has no trash.
        if std::fs::symlink_metadata(dest).is_err() {
            return None;
        }
        match self.bin(dest) {
            Some(Ok(item)) => {
                self.undone(Undone::Displaced(item.clone()));
                Some(item)
            }
            Some(Err(err)) => {
                self.warn(dest, format!("could not be moved to the trash: {err}"));
                self.undone(Undone::Lost(dest.to_path_buf()));
                None
            }
            // Nowhere on this filesystem can hold what is about to be
            // replaced. One warning for the job rather than one per file, and
            // a record saying which file so undo can name what it cannot put
            // back rather than quietly restoring less than it claims.
            None => {
                if !self.warned_no_trash {
                    self.warned_no_trash = true;
                    self.warn(
                        dest,
                        "nothing here can hold a trash, so replaced files cannot be restored"
                            .to_string(),
                    );
                }
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
