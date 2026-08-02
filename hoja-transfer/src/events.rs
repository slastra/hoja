use std::path::PathBuf;
use std::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    KeepBoth,
}

/// UI → worker answer. `CancelJob` exists so a user cancelling from the prompt
/// itself resolves the wait; the worker also polls the cancel flag while waiting,
/// so a strip-button cancel or a dropped sender can never hang the job.
#[derive(Debug, Clone, Copy)]
pub enum ConflictDecision {
    Apply {
        choice: ConflictChoice,
        apply_to_all: bool,
    },
    CancelJob,
}

/// Where in the per-file pipeline an error occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Walk,
    CreateDir,
    Open,
    Read,
    Write,
    Rename,
    Metadata,
    Symlink,
    Hardlink,
    DeleteSource,
    Sync,
}

#[derive(Debug)]
pub struct TransferError {
    pub stage: Stage,
    pub source: std::io::Error,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.source)
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Worker → UI. Drained via `JobHandle::try_recv_event` on the UI's poll timer.
#[derive(Debug)]
pub enum Event {
    /// Dest exists and no sticky policy is set. The worker is now blocked
    /// (cancel-aware) until `reply` is answered or the job is cancelled.
    Conflict {
        src: PathBuf,
        dest: PathBuf,
        reply: mpsc::Sender<ConflictDecision>,
    },
    /// A file failed; the job continues. Also queued into the final summary.
    FileError { path: PathBuf, error: TransferError },
    /// Fail-soft metadata loss (e.g. xattrs on FAT). Informational.
    Warning { path: PathBuf, detail: String },
    /// Always the final event on the channel, exactly once.
    Done(JobSummary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    CompletedWithErrors,
    Cancelled,
}

/// Which mechanisms actually ran. Public so dispatch behavior is table-testable.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TierStats {
    /// Tier 0: whole files or directories moved by `rename()`.
    pub renames: u64,
    /// Tier 1: files cloned with FICLONE.
    pub reflinks: u64,
    /// Tier 2 fast path: files copied via `copy_file_range`.
    pub copy_file_range: u64,
    /// Tier 2 fallback: files copied via read/write loop.
    pub read_write: u64,
    pub hardlinks: u64,
    pub symlinks: u64,
}

#[derive(Debug)]
pub struct JobSummary {
    pub outcome: Outcome,
    pub errors: Vec<(PathBuf, TransferError)>,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub bytes_copied: u64,
    pub stats: TierStats,
}
