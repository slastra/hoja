//! Putting a transfer back.
//!
//! In its own binary for the same reason `overwrite.rs` is: undo takes files
//! away by moving them to the trash rather than unlinking them, so every test
//! here needs `XDG_DATA_HOME` pointed somewhere disposable, and every test here
//! takes the guard that makes pointing it sound.

mod common;

use std::os::unix::fs::MetadataExt;

use common::*;
use hoja_transfer::{ConflictChoice, ConflictDecision, Outcome, Undone, spawn_job, spawn_undo};

/// Run a transfer, then undo it, and hand back both summaries.
fn there_and_back(
    spec: hoja_transfer::JobSpec,
    answer: impl Fn() -> ConflictDecision,
) -> (hoja_transfer::JobSummary, hoja_transfer::JobSummary) {
    let handle = spawn_job(spec).unwrap();
    let (_, forward) = drain(&handle, answer);
    let back = spawn_undo("it".to_string(), forward.undone.clone()).unwrap();
    let (_, reverse) = drain(&back, never_conflict);
    (forward, reverse)
}

/// Every path under `dir`, relative and sorted, so two trees can be compared.
fn shape(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            found.push(
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn undoing_a_copy_leaves_the_destination_as_it_was() {
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    write_file(&tree, "a.bin", b"aaa");
    write_file(&tree.join("nested"), "b.bin", b"bbb");
    write_file(dst_dir.path(), "untouched.txt", b"mine");

    let before = shape(dst_dir.path());
    let (_, reverse) = there_and_back(
        copy_spec(vec![tree.clone()], dst_dir.path()),
        never_conflict,
    );

    assert_eq!(reverse.outcome, Outcome::Completed);
    assert_eq!(
        shape(dst_dir.path()),
        before,
        "the destination is as it was"
    );
    assert!(tree.exists(), "and a copy left its source alone");
}

#[test]
fn undoing_a_same_filesystem_move_is_renames() {
    // Not a copy back. The forward move was one rename, and so is this: if it
    // fell through to copying, the bytes of a 200 GB tree would go over the
    // bus twice for a keystroke.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    write_file(&tree.join("nested"), "b.bin", b"bbb");

    let (_, reverse) = there_and_back(
        move_spec(vec![tree.clone()], dst_dir.path()),
        never_conflict,
    );

    assert_eq!(reverse.outcome, Outcome::Completed);
    assert_eq!(reverse.stats.renames, 1, "one call put the subtree back");
    assert!(tree.join("nested/b.bin").exists(), "back where it started");
    assert!(!dst_dir.path().join("tree").exists());
}

#[test]
fn undoing_an_overwrite_puts_the_old_file_back() {
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "z.txt", b"new");
    let dest = write_file(dst_dir.path(), "z.txt", b"old");

    let (_, reverse) = there_and_back(copy_spec(vec![src], dst_dir.path()), || {
        ConflictDecision::Apply {
            choice: ConflictChoice::Overwrite,
            apply_to_all: false,
        }
    });

    assert_eq!(reverse.outcome, Outcome::Completed);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"old",
        "the file that was replaced is back"
    );
}

#[test]
fn undoing_a_cross_filesystem_move_copies_it_back() {
    // The expensive inverse, and there is no cheaper one: the source was
    // deleted after the copy landed, so putting it back is the same copy the
    // other way. It goes through the same ladder rather than a plain read and
    // write, which is why undo is a Worker at all.
    let _trash = trash_env();
    let src_dir = tmpfs_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "far.bin", &vec![7u8; 64 * 1024]);

    let (forward, reverse) =
        there_and_back(move_spec(vec![src.clone()], dst_dir.path()), never_conflict);

    assert_eq!(forward.stats.renames, 0, "it really did cross a boundary");
    assert_eq!(reverse.outcome, Outcome::Completed);
    assert!(src.exists(), "back on the filesystem it came from");
    assert_eq!(std::fs::read(&src).unwrap().len(), 64 * 1024);
    assert!(!dst_dir.path().join("far.bin").exists());
}

#[test]
fn undo_refuses_a_file_that_has_been_edited_since() {
    // Undo is a guess about what someone wanted. Removing work they did after
    // the transfer is the one way it can be actively harmful, so it checks
    // what it is about to take away and declines when the answer changed.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "edit.txt", b"copied");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);

    let landed = dst_dir.path().join("edit.txt");
    write_file(dst_dir.path(), "edit.txt", b"and then edited by hand");

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::CompletedWithErrors);
    assert_eq!(
        std::fs::read(&landed).unwrap(),
        b"and then edited by hand",
        "the edit survives"
    );
    assert_eq!(reverse.errors.len(), 1);
    assert!(
        reverse.errors[0].1.to_string().contains("edited since"),
        "and it says why: {}",
        reverse.errors[0].1
    );
    assert_eq!(
        reverse.undone.len(),
        1,
        "the record stays outstanding, so a second press can try again"
    );
}

#[test]
fn undo_takes_a_copy_to_the_trash_rather_than_deleting_it() {
    // So an undo that guessed wrong is itself recoverable.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "bin.txt", b"contents");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);
    let landed = dst_dir.path().join("bin.txt");
    let ino = std::fs::symlink_metadata(&landed).unwrap().ino();

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::Completed);
    assert!(!landed.exists());
    let files = std::path::Path::new(&*_trash).join("Trash/files");
    let recovered = std::fs::read_dir(&files)
        .unwrap()
        .flatten()
        .find(|e| e.metadata().map(|m| m.ino()).ok() == Some(ino));
    assert!(
        recovered.is_some(),
        "the same inode is in the trash, not unlinked"
    );
}

#[test]
fn a_record_for_something_already_gone_is_not_an_error() {
    // Someone deleted the copy themselves before pressing ctrl-z. The wanted
    // state is the state, so there is nothing to report.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "vanish.txt", b"x");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);
    std::fs::remove_file(dst_dir.path().join("vanish.txt")).unwrap();

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::Completed);
    assert!(reverse.undone.is_empty());
}

#[test]
fn what_it_could_not_reverse_comes_back_in_order() {
    // The outstanding records are handed back the way they were recorded, not
    // the way they were walked, so a second press replays them exactly as the
    // first one would have.
    let _trash = trash_env();
    let dst_dir = ext4_dir();
    let a = dst_dir.path().join("a");
    let b = dst_dir.path().join("b");

    let records = vec![Undone::Lost(a.clone()), Undone::Lost(b.clone())];
    let back = spawn_undo("it".to_string(), records).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::CompletedWithErrors);
    assert_eq!(
        reverse.undone,
        vec![Undone::Lost(a), Undone::Lost(b)],
        "recorded order, not reversed"
    );
}
