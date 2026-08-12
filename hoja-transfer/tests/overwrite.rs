//! Transfers onto a destination that is already occupied.
//!
//! Apart from `engine.rs` because of what an overwrite is about to become: the
//! old file is moved to the trash rather than unlinked, so every test here
//! writes into a trash directory. That needs `XDG_DATA_HOME` pointed somewhere
//! disposable, and pointing it is a `set_var` on a process the test harness is
//! already running threads in — sound only while nothing else in the binary
//! reads the environment beside it. `common::trash_env` holds that line by
//! serialising the tests that take it, which works here because *every* test in
//! this file takes it, and would not work in `engine.rs`, where three in
//! thirty would.

mod common;

use common::*;
use hoja_transfer::{ConflictChoice, ConflictDecision, JobPolicy, JobSpec, Outcome, spawn_job};

#[test]
fn overwrite_and_keep_both() {
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "x.tar.gz", b"new");
    write_file(dst_dir.path(), "x.tar.gz", b"old");

    // Overwrite
    let handle = spawn_job(copy_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });
    assert_eq!(summary.files_copied, 1);
    assert_eq!(
        std::fs::read(dst_dir.path().join("x.tar.gz")).unwrap(),
        b"new"
    );

    // KeepBoth with multi-part extension naming
    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::KeepBoth,
        apply_to_all: false,
    });
    assert_eq!(summary.files_copied, 1);
    assert_eq!(
        std::fs::read(dst_dir.path().join("x (copy).tar.gz")).unwrap(),
        b"new"
    );
}

#[test]
fn preset_policy_never_prompts() {
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "y.txt", b"new");
    write_file(dst_dir.path(), "y.txt", b"old");

    let handle = spawn_job(JobSpec {
        policy: JobPolicy {
            conflict: Some(ConflictChoice::Overwrite),
            ..Default::default()
        },
        ..copy_spec(vec![src], dst_dir.path())
    })
    .unwrap();
    let (events, summary) = drain(&handle, never_conflict);
    assert_eq!(conflict_count(&events), 0);
    assert_eq!(std::fs::read(dst_dir.path().join("y.txt")).unwrap(), b"new");
    assert_eq!(summary.outcome, Outcome::Completed);
}

#[test]
fn move_overwrite_replaces_and_keep_both_renames() {
    // The move path renames without replacing, so an Overwrite decision has to
    // clear the destination explicitly. Both outcomes are checked here because
    // getting the first wrong silently refuses, and the second silently clobbers.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();

    let a = write_file(src_dir.path(), "m.txt", b"new");
    write_file(dst_dir.path(), "m.txt", b"old");
    let handle = spawn_job(move_spec(vec![a.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });
    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(std::fs::read(dst_dir.path().join("m.txt")).unwrap(), b"new");
    assert!(!a.exists(), "source removed after a move");

    let b = write_file(src_dir.path(), "m.txt", b"second");
    let handle = spawn_job(move_spec(vec![b], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::KeepBoth,
        apply_to_all: false,
    });
    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(std::fs::read(dst_dir.path().join("m.txt")).unwrap(), b"new");
    assert_eq!(
        std::fs::read(dst_dir.path().join("m (copy).txt")).unwrap(),
        b"second"
    );
}
