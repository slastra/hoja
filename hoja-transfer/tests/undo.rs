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

    assert_eq!(
        reverse.outcome,
        Outcome::Completed,
        "errors: {:?}",
        reverse.errors
    );
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

#[test]
fn undoing_a_cross_filesystem_directory_move_puts_the_files_back() {
    // The collapse is a copy-only shortcut. A move deletes its sources as it
    // goes, so a single `CreatedDir` standing for the destination would say
    // nothing about where any of it came from — undo would take the copies
    // away and leave the source empty, which is the transfer done twice
    // rather than undone.
    let _trash = trash_env();
    let src_dir = tmpfs_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    write_file(&tree, "a.bin", b"aaa");
    write_file(&tree.join("nested"), "b.bin", b"bbb");

    let (forward, reverse) = there_and_back(
        move_spec(vec![tree.clone()], dst_dir.path()),
        never_conflict,
    );

    assert_eq!(forward.stats.renames, 0, "it really did cross a boundary");
    assert_eq!(
        reverse.outcome,
        Outcome::Completed,
        "errors: {:?}",
        reverse.errors
    );
    assert_eq!(
        std::fs::read(tree.join("a.bin")).unwrap(),
        b"aaa",
        "back on the filesystem it came from"
    );
    assert_eq!(std::fs::read(tree.join("nested/b.bin")).unwrap(), b"bbb");
    assert!(
        !dst_dir.path().join("tree").exists(),
        "and gone from where it was moved to"
    );
}

#[test]
fn undoing_a_move_that_merged_puts_the_source_directory_back() {
    // The destination already held a directory of the same name, so nothing
    // was created there and the children were recorded one by one. Their
    // parent on the *source* side was emptied and removed, and without a
    // record of that every one of them fails ENOENT on a directory that is no
    // longer there.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("photos");
    std::fs::create_dir(&tree).unwrap();
    write_file(&tree, "one.jpg", b"1");
    write_file(&tree, "two.jpg", b"2");
    std::fs::create_dir(dst_dir.path().join("photos")).unwrap();

    let (_, reverse) = there_and_back(
        move_spec(vec![tree.clone()], dst_dir.path()),
        never_conflict,
    );

    assert_eq!(
        reverse.outcome,
        Outcome::Completed,
        "errors: {:?}",
        reverse.errors
    );
    assert_eq!(std::fs::read(tree.join("one.jpg")).unwrap(), b"1");
    assert_eq!(std::fs::read(tree.join("two.jpg")).unwrap(), b"2");
    assert!(!dst_dir.path().join("photos/one.jpg").exists());
}

#[test]
fn undo_refuses_a_directory_that_has_been_added_to() {
    // The copy's record stands for the whole subtree, so removing it would
    // take anything put there since along with it. A directory's mtime moves
    // when an entry is added, which is what this notices.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    write_file(&tree, "a.bin", b"aaa");

    let handle = spawn_job(copy_spec(vec![tree], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);

    let landed = dst_dir.path().join("tree");
    let mine = write_file(&landed, "mine.txt", b"work i did afterwards");

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::CompletedWithErrors);
    assert!(mine.exists(), "my file is still there");
    assert!(landed.exists(), "and so is the directory holding it");
    assert_eq!(
        reverse.undone.len(),
        1,
        "the record stays outstanding rather than being called done"
    );
}

#[test]
fn undo_refuses_to_delete_where_it_cannot_bin() {
    // Undo trashes rather than unlinks so that a wrong guess costs nothing.
    // Falling back to deleting where no trash exists made the one case that
    // cannot be taken back the one case with no safety net.
    let _trash = trash_env();
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "keep.txt", b"contents");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);
    let landed = dst_dir.path().join("keep.txt");

    let blocked = src_dir.path().join("not-a-dir");
    std::fs::write(&blocked, b"").unwrap();
    let previous = std::env::var_os("XDG_DATA_HOME");
    // Safe under the guard every test in this file takes.
    unsafe { std::env::set_var("XDG_DATA_HOME", &blocked) };

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    match previous {
        Some(value) => unsafe { std::env::set_var("XDG_DATA_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }

    assert_eq!(reverse.outcome, Outcome::CompletedWithErrors);
    assert!(
        landed.exists(),
        "left in place rather than deleted with no way back"
    );
    assert_eq!(reverse.undone.len(), 1, "and still owed");
}

#[test]
fn undoing_a_cross_filesystem_move_refuses_an_occupied_original() {
    // `rename_no_replace` refuses; the copy-back it falls through to ends in a
    // plain rename and would not, so something new at the original path was
    // silently destroyed by a keystroke meant to be conservative.
    let _trash = trash_env();
    let src_dir = tmpfs_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "report.pdf", b"original");

    let handle = spawn_job(move_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, forward) = drain(&handle, never_conflict);
    assert_eq!(forward.stats.renames, 0, "it crossed a boundary");

    // Somebody put a different file back at the old name in the meantime.
    write_file(src_dir.path(), "report.pdf", b"something else entirely");

    let back = spawn_undo("it".to_string(), forward.undone).unwrap();
    let (_, reverse) = drain(&back, never_conflict);

    assert_eq!(reverse.outcome, Outcome::CompletedWithErrors);
    assert_eq!(
        std::fs::read(&src).unwrap(),
        b"something else entirely",
        "the newer file survives"
    );
    assert!(
        dst_dir.path().join("report.pdf").exists(),
        "and the copy is left alone rather than half-moved"
    );
}
