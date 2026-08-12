//! Shared fixtures for engine integration tests.
//!
//! Filesystem boundaries, which several tests depend on rather than merely
//! benefit from:
//! - `CARGO_TARGET_TMPDIR` lives under `target/`, on whatever filesystem the
//!   checkout is on: reflink attempts fail there, exercising the ladder.
//! - `/dev/shm` is tmpfs: crossing into it exercises both the rename EXDEV path
//!   and the cross-fs-type `copy_file_range` fallback.

// Compiled once per test binary, and no binary uses all of it: `trash.rs` wants
// only `trash_env`, `engine.rs` wants everything but. Without this each binary
// warns about the half it does not use, and CI runs `-D warnings`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use hoja_transfer::{
    ConflictDecision, Event, JobHandle, JobPolicy, JobSpec, JobSummary, Operation,
};

pub fn ext4_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir on target fs")
}

/// A directory on a filesystem that is *not* the one `ext4_dir` returns.
///
/// `/dev/shm` and not `std::env::temp_dir()`, because the boundary is the whole
/// point and `/tmp` is not reliably one. On a developer machine it is usually
/// tmpfs; on a stock CI runner it is the same ext4 as the checkout, and there
/// the rename that `tier0_exdev_falls_to_copy_and_caches` requires to fail
/// quietly succeeds, so the test asserts a fallback that never ran. `/dev/shm`
/// is tmpfs on every Linux.
pub fn tmpfs_dir() -> tempfile::TempDir {
    let shm = Path::new("/dev/shm");
    if shm.is_dir()
        && let Ok(dir) = tempfile::tempdir_in(shm)
    {
        return dir;
    }
    tempfile::tempdir().expect("tempdir on /tmp")
}

/// Serialises the tests and points `XDG_DATA_HOME` somewhere disposable.
///
/// Two things depend on this, and the second is easy to miss.
///
/// It keeps the developer's real trash out of the run, which is what it was
/// written for. But `trash` only ever renames, never copies, and falls back to
/// a volume trash at the *destination's* top directory when the home trash is
/// on another filesystem — which for a file under `CARGO_TARGET_TMPDIR` means
/// trying to make `.Trash-$uid` at the root of the filesystem the checkout is
/// on. So the root here is under `CARGO_TARGET_TMPDIR` too, sharing a device
/// with `ext4_dir`, and a destination there gets an ordinary home trash.
///
/// **Every test in a binary that calls this has to call it**, because the
/// safety of the `set_var` below rests on nothing else in the process reading
/// the environment while it happens. That is why the tests that overwrite live
/// in `overwrite.rs` rather than beside the rest of the engine's.
pub fn trash_env() -> MutexGuard<'static, PathBuf> {
    static ENV: OnceLock<Mutex<PathBuf>> = OnceLock::new();
    ENV.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("hoja-trash-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Safety: set once, before any test body runs, under the lock that
        // every test in this file takes.
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        Mutex::new(root)
    })
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

pub fn copy_spec(sources: Vec<PathBuf>, dest: &Path) -> JobSpec {
    JobSpec {
        op: Operation::Copy,
        sources,
        dest_dir: dest.to_path_buf(),
        policy: JobPolicy::default(),
    }
}

pub fn move_spec(sources: Vec<PathBuf>, dest: &Path) -> JobSpec {
    JobSpec {
        op: Operation::Move,
        ..copy_spec(sources, dest)
    }
}

pub fn never_conflict() -> ConflictDecision {
    panic!("unexpected conflict event")
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

/// Poll until `cond` holds, and fail saying `what` if it never does.
///
/// A deadline rather than a fixed sleep: the thing being waited for is a worker
/// thread reaching a point, which is fast on a developer machine and not always
/// fast on a runner.
pub fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
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
