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
    ConflictChoice, Event, JobSummary, Operation, Outcome, Stage, TierStats, TransferError,
};
use crate::meta;
use crate::sys::{self, MountKey};

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
/// `should_scan`), in which case they grow as the walk finds things —
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

    /// Honored between files in M1.
    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Validate cheaply, spawn one worker thread, return immediately.
pub fn spawn_job(spec: JobSpec) -> std::io::Result<JobHandle> {
    if !spec.dest_dir.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("destination is not a directory: {}", spec.dest_dir.display()),
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
        }
    }

    /// Whether to count the tree before transferring it.
    ///
    /// Worth it whenever bytes will actually be copied: the walk costs about a
    /// second and a half for 86,000 files against a copy of the same tree that
    /// costs minutes, and it buys a real denominator — without it the progress
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
        // size to a total that the transfer never counts towards done — and
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
        };
        let _ = self.events.send(Event::Done(summary));
    }

    fn check_pause_cancel(&self) -> bool {
        while self.pause.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
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
        let dest = match self.resolve_dest(src, dest) {
            DestPlan::Proceed(d) => d,
            DestPlan::Skip => {
                self.files_skipped += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                return Step::Ok;
            }
            DestPlan::Cancel => return Step::Cancelled,
        };
        // Overwrite of an existing entry: remove it first — symlink() has no
        // replace semantics.
        let _ = std::fs::remove_file(&dest);
        match meta::copy_symlink(src, &dest) {
            Ok(()) => {
                self.stats.symlinks += 1;
                self.files_copied += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                if self.spec.op == Operation::Move
                    && let Err(err) = std::fs::remove_file(src) {
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
            && let (Ok(src_key), Some(dst_key)) = (sys::mount_key(src), self.dest_mount)
            && self.caps.rename_worth_trying(src_key, dst_key)
        {
            match sys::rename_no_replace(src, dest) {
                Ok(()) => {
                    self.stats.renames += 1;
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
        if !dest.is_dir()
            && let Err(err) = std::fs::create_dir(dest) {
                self.queue_error(dest, Stage::CreateDir, err);
                return self.continue_or_fatal();
            }

        // Enumerate children first so totals grow ahead of processing (this IS
        // the one walk — gate counters for M3 accumulate on these same adds).
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
            if self
                .errors
                .iter()
                .any(|(p, _)| p.starts_with(&child_src))
            {
                child_failed = true;
            }
        }

        // Metadata after children: writes inside would clobber the times.
        let outcome = meta::apply_dir_meta(src_meta, dest);
        for w in outcome.warnings {
            self.warn(dest, w);
        }

        // Move: remove the now-empty source dir, but never when a child failed —
        // that would orphan whatever is still inside.
        if self.spec.op == Operation::Move && !child_failed
            && let Err(err) = std::fs::remove_dir(src) {
                self.queue_error(src, Stage::DeleteSource, err);
            }
        Step::Ok
    }

    fn transfer_file(&mut self, src: &Path, dest: &Path, src_meta: &std::fs::Metadata) -> Step {
        self.progress.set_current(src);

        // Tier 0: move via rename.
        if self.spec.op == Operation::Move
            && let (Ok(src_key), Some(dst_key)) = (sys::mount_key(src), self.dest_mount)
            && self.caps.rename_worth_trying(src_key, dst_key)
        {
            // rename() clobbers silently, so conflicts resolve BEFORE the
            // attempt — and the attempt itself refuses to replace, so a file
            // appearing in between is re-resolved rather than destroyed.
            let mut planned = match self.resolve_dest(src, dest) {
                DestPlan::Proceed(d) => d,
                DestPlan::Skip => {
                    self.files_skipped += 1;
                    self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    return Step::Ok;
                }
                DestPlan::Cancel => return Step::Cancelled,
            };
            // An Overwrite decision means the user asked for the replacement,
            // so unlink first; NOREPLACE would otherwise refuse it.
            if planned == *dest && std::fs::symlink_metadata(&planned).is_ok() {
                let _ = std::fs::remove_file(&planned);
            }
            let mut renamed = sys::rename_no_replace(src, &planned);
            if matches!(&renamed, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists) {
                // Raced: ask again against the destination that now exists.
                match self.resolve_dest(src, &planned) {
                    DestPlan::Proceed(d) => {
                        planned = d;
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
                    // destination.
                    return self.copy_file_inner(src, &planned, src_meta, true);
                }
                Err(err) => {
                    self.queue_error(src, Stage::Rename, err);
                    return self.continue_or_fatal();
                }
            }
        }

        let planned = match self.resolve_dest(src, dest) {
            DestPlan::Proceed(d) => d,
            DestPlan::Skip => {
                self.files_skipped += 1;
                self.progress.files_done.fetch_add(1, Ordering::Relaxed);
                return Step::Ok;
            }
            DestPlan::Cancel => return Step::Cancelled,
        };
        self.copy_file_inner(src, &planned, src_meta, self.spec.op == Operation::Move)
    }

    /// The copy path shared by Copy jobs and cross-fs Move fallback.
    /// `delete_source` only ever happens after verify + rename.
    fn copy_file_inner(
        &mut self,
        src: &Path,
        dest: &Path,
        src_meta: &std::fs::Metadata,
        delete_source: bool,
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
                let dest_existed = dest.exists();
                if dest_existed && let Err(err) = rustix::fs::fsync(&tmp) {
                    drop(tmp);
                    let _ = std::fs::remove_file(&tmp_path);
                    self.queue_error(dest, Stage::Write, err.into());
                    return self.continue_or_fatal();
                }
                drop(tmp);

                if let Err(err) = std::fs::rename(&tmp_path, dest) {
                    let _ = std::fs::remove_file(&tmp_path);
                    self.queue_error(dest, Stage::Rename, err);
                    return self.continue_or_fatal();
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
        if let (Ok(src_key), Some(dst_key)) = (sys::mount_key(src), self.dest_mount)
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
        )
    }

    fn resolve_dest(&mut self, src: &Path, dest: &Path) -> DestPlan {
        // lstat: a dangling symlink at dest is still a conflict.
        if std::fs::symlink_metadata(dest).is_err() {
            return DestPlan::Proceed(dest.to_path_buf());
        }
        self.progress.set_phase(Phase::AwaitingConflict);
        let resolution = self.conflicts.resolve(src, dest);
        self.progress.set_phase(Phase::Transferring);
        match resolution {
            Resolution::CancelJob => DestPlan::Cancel,
            Resolution::Proceed(ConflictChoice::Skip) => DestPlan::Skip,
            Resolution::Proceed(ConflictChoice::Overwrite) => DestPlan::Proceed(dest.to_path_buf()),
            Resolution::Proceed(ConflictChoice::KeepBoth) => {
                for attempt in 1..1000 {
                    let candidate = sys::keep_both_name(dest, attempt);
                    if std::fs::symlink_metadata(&candidate).is_err() {
                        return DestPlan::Proceed(candidate);
                    }
                }
                DestPlan::Skip
            }
        }
    }
}

enum DestPlan {
    Proceed(PathBuf),
    Skip,
    Cancel,
}

use std::os::unix::fs::OpenOptionsExt;
