//! Shared fixtures for engine integration tests.
//!
//! Filesystem boundaries on the dev machine this suite is tuned for:
//! - `CARGO_TARGET_TMPDIR` lives under `target/` on the root filesystem (ext4
//!   here) — reflink attempts fail there, exercising the fallback ladder.
//! - `std::env::temp_dir()` is tmpfs — crossing into it exercises both the
//!   rename EXDEV path and the cross-fs-type `copy_file_range` fallback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hoja_transfer::{ConflictDecision, Event, JobHandle, JobSummary};

pub fn ext4_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir on target fs")
}

pub fn tmpfs_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir on /tmp")
}

pub fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// Drain a job to completion, answering every conflict with `answer`.
/// Returns all non-Done events plus the summary.
pub fn drain(
    handle: &JobHandle,
    answer: impl Fn() -> ConflictDecision,
) -> (Vec<Event>, JobSummary) {
    let mut events = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);

    loop {
        assert!(std::time::Instant::now() < deadline, "job did not finish");
        match handle.try_recv_event() {
            Some(Event::Done(summary)) => return (events, summary),
            Some(Event::Conflict { src, dest, reply }) => {
                reply.send(answer()).ok();
                events.push(Event::Conflict {
                    src,
                    dest,
                    reply: std::sync::mpsc::channel().0, // placeholder, already answered
                });
            }
            Some(other) => events.push(other),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

pub fn conflict_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Conflict { .. }))
        .count()
}

pub fn no_partials_under(dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !hoja_transfer::is_partial_name(&name),
                "orphaned partial file: {}",
                entry.path().display()
            );
            if entry.path().is_dir() {
                stack.push(entry.path());
            }
        }
    }
}
