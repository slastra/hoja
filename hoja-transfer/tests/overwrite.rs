//! Transfers onto a destination that is already occupied.
//!
//! Apart from `engine.rs` because of what an overwrite is: the old file is
//! moved to the trash rather than unlinked, so every test here writes into a
//! trash directory. That needs `XDG_DATA_HOME` pointed somewhere
//! disposable, and pointing it is a `set_var` on a process the test harness is
//! already running threads in — sound only while nothing else in the binary
//! reads the environment beside it. `common::trash_env` holds that line by
//! serialising the tests that take it, which works here because *every* test in
//! this file takes it, and would not work in `engine.rs`, where three in
//! thirty would.

mod common;

use common::*;
use hoja_transfer::{
    ConflictChoice, ConflictDecision, JobPolicy, JobSpec, Outcome, Undone, restore, spawn_job,
};

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
fn an_overwrite_records_where_the_old_file_went() {
    // The point of the whole change: what the rename replaces is not gone, it
    // is in the trash, and the record says exactly which entry to put back.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "z.txt", b"new");
    let dest = write_file(dst_dir.path(), "z.txt", b"old");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });

    assert_eq!(summary.outcome, Outcome::Completed);
    assert!(summary.undoable);
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    match summary.undone.as_slice() {
        [Undone::Displaced(item), Undone::Created { path, .. }] => {
            assert_eq!(item.original, dest, "it knows where to put it back");
            assert_eq!(
                std::fs::read(&item.file).unwrap(),
                b"old",
                "and the old bytes are still there to put"
            );
            assert_eq!(path, &dest);
        }
        other => panic!("expected the old file displaced and the new one made, got {other:?}"),
    }
    // Displaced before created, so undo replaying in reverse takes the new
    // file away before `restore` tries to rename onto the name it holds.
    assert!(
        restore(match &summary.undone[0] {
            Undone::Displaced(item) => item,
            _ => unreachable!(),
        })
        .is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
    );
}

#[test]
fn a_destination_with_no_trash_still_overwrites() {
    // The escape hatch, and it has to be an escape hatch: `trash` refuses
    // rather than copying across a filesystem, so a FAT stick has nowhere to
    // put the old file. Failing the paste there would be a regression nobody
    // asked for, so it overwrites as it always did, warns once, and records
    // the loss rather than pretending the job can be undone.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "n.txt", b"new");
    let dest = write_file(dst_dir.path(), "n.txt", b"old");

    // A data home that cannot be made into a directory, so resolving a trash
    // for the destination fails the way an unwritable volume root does.
    // Safe under the guard this file's tests all take: nothing else in the
    // process is reading the environment while it changes.
    let blocked = src_dir.path().join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let previous = std::env::var_os("XDG_DATA_HOME");
    unsafe { std::env::set_var("XDG_DATA_HOME", &blocked) };

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (events, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });

    match previous {
        Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"new",
        "the overwrite still happened"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, hoja_transfer::Event::Warning { .. }))
            .count(),
        1,
        "one warning for the job, not one per file"
    );
    assert!(
        summary.undone.contains(&Undone::Lost(dest.clone())),
        "and it says which file it could not keep: {:?}",
        summary.undone
    );
}

#[test]
fn overwriting_the_same_name_repeatedly_keeps_every_version() {
    // Names repeat: syncing one tree over another overwrites a thousand files
    // called `index.html`. Claiming walks `name`, `name.2`, `name.3`, … so
    // each one starts where the last finished rather than from the top, which
    // is what keeps a job of these linear instead of quadratic. Correctness is
    // what is asserted here — that the shortcut never hands back a name that
    // is taken, and never loses a version.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();

    let mut displaced = Vec::new();
    for round in 0..8u8 {
        let src = write_file(src_dir.path(), "same.txt", &[b'a' + round]);
        let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
        let (_, summary) = drain(&handle, || ConflictDecision::Apply {
            choice: ConflictChoice::Overwrite,
            apply_to_all: false,
        });
        for record in &summary.undone {
            if let Undone::Displaced(item) = record {
                displaced.push((item.file.clone(), round));
            }
        }
    }

    assert_eq!(
        displaced.len(),
        7,
        "every round after the first displaced one"
    );
    let names: std::collections::BTreeSet<_> = displaced.iter().map(|(f, _)| f.clone()).collect();
    assert_eq!(names.len(), 7, "and each landed on a name of its own");
    for (file, round) in &displaced {
        assert_eq!(
            std::fs::read(file).unwrap(),
            vec![b'a' + round - 1],
            "the version it replaced, not some other round's"
        );
    }
}

#[test]
fn keeping_both_leaves_the_job_undoable() {
    // The counterpart, and the reason the flag is set where the destination is
    // resolved rather than wherever a conflict was raised: KeepBoth answers a
    // conflict too, and destroys nothing.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "k.txt", b"new");
    write_file(dst_dir.path(), "k.txt", b"old");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::KeepBoth,
        apply_to_all: false,
    });

    assert!(summary.undoable);
    match summary.undone.as_slice() {
        [hoja_transfer::Undone::Created { path, .. }] => {
            assert_eq!(path, &dst_dir.path().join("k (copy).txt"));
        }
        other => panic!("expected the copy it actually wrote, got {other:?}"),
    }
    assert_eq!(std::fs::read(dst_dir.path().join("k.txt")).unwrap(), b"old");
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

#[test]
fn an_overwrite_that_fails_leaves_the_original_where_it_was() {
    // The destination is only moved out of the way once the replacement is
    // written and durable. Displacing when the conflict was *answered* meant a
    // copy that then failed to open its source had already emptied the
    // destination: the job reported an error about the source while the file
    // the user actually had went quietly to the trash.
    use std::os::unix::fs::PermissionsExt;

    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "q.txt", b"new");
    let dest = write_file(dst_dir.path(), "q.txt", b"old");
    // Unreadable, so the copy fails after the conflict is answered.
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();

    let handle = spawn_job(copy_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(summary.outcome, Outcome::CompletedWithErrors);
    assert!(
        dest.exists(),
        "the file that was there is still there, not only in the trash"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"old", "and unchanged");
    assert!(
        !summary
            .undone
            .iter()
            .any(|r| matches!(r, Undone::Displaced(_))),
        "and nothing was displaced, because nothing replaced it"
    );
}

#[test]
fn replacing_a_symlink_where_no_trash_exists_still_replaces_it() {
    // The unlink that clears the name runs on what the user chose, not on
    // whether the trash accepted the old entry. Keying it on the displacement
    // skipped it in exactly the case where the old entry was still there, and
    // symlinkat then failed EEXIST on a replacement that had been authorised.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let target = write_file(src_dir.path(), "target.txt", b"t");
    let link = src_dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let dest = write_file(dst_dir.path(), "link", b"in the way");

    let blocked = src_dir.path().join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let previous = std::env::var_os("XDG_DATA_HOME");
    // Safe under the guard every test in this file takes.
    unsafe { std::env::set_var("XDG_DATA_HOME", &blocked) };

    let handle = spawn_job(copy_spec(vec![link], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Overwrite,
        apply_to_all: false,
    });

    match previous {
        Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }

    assert_eq!(summary.outcome, Outcome::Completed, "{:?}", summary.errors);
    assert!(
        std::fs::symlink_metadata(&dest).unwrap().is_symlink(),
        "the replacement the user asked for actually happened"
    );
}
