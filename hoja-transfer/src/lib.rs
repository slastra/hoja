//! Tiered file transfer engine for the pane file manager.
//!
//! Design: boring fast paths are
//! the default: rename for same-mount moves, FICLONE reflink for same-fs
//! copies, `copy_file_range` for the rest, with attempt-and-fallback instead of
//! filesystem sniffing, and a correctness layer (metadata, symlinks, hardlinks,
//! sparse files, atomic temp+rename, conflict prompts, error queueing,
//! syncfs-on-removable) applied uniformly.
//!
//! The crate is std-only: one worker thread per job, `mpsc` events, atomic
//! progress counters. The UI never blocks on the engine and the engine never
//! holds UI types.

mod conflict;
mod copy;
mod dispatch;
mod events;
mod job;
mod meta;
mod sys;
mod trash;

pub use events::{
    ConflictChoice, ConflictDecision, Event, JobSummary, Operation, Outcome, Stage, TierStats,
    TransferError, Undone,
};
pub use job::{JobHandle, JobId, JobPolicy, JobSpec, Phase, Progress, spawn_job, spawn_undo};
pub use sys::{is_partial_name, partial_path, rename_no_replace, same_filesystem, stem_end};
pub use trash::{TrashDir, TrashedItem, restore, trash};
