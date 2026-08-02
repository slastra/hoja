//! Integration tests for the tier ladder and correctness layer.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use common::*;
use hoja_transfer::{
    ConflictChoice, ConflictDecision, JobPolicy, JobSpec, Operation, Outcome, spawn_job,
};

fn copy_spec(sources: Vec<std::path::PathBuf>, dest: &Path) -> JobSpec {
    JobSpec {
        op: Operation::Copy,
        sources,
        dest_dir: dest.to_path_buf(),
        policy: JobPolicy::default(),
    }
}

fn move_spec(sources: Vec<std::path::PathBuf>, dest: &Path) -> JobSpec {
    JobSpec {
        op: Operation::Move,
        ..copy_spec(sources, dest)
    }
}

fn never_conflict() -> ConflictDecision {
    panic!("unexpected conflict event")
}

unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

// ---- Tier 0 ---------------------------------------------------------------

#[test]
fn tier0_same_fs_move_is_a_rename() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "a.txt", b"hello");
    let src_ino = std::fs::metadata(&src).unwrap().ino();

    let handle = spawn_job(move_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.renames, 1);
    assert!(!src.exists());
    let dest = dst_dir.path().join("a.txt");
    assert_eq!(std::fs::metadata(&dest).unwrap().ino(), src_ino, "same inode = rename");
}

#[test]
fn tier0_exdev_falls_to_copy_and_caches() {
    let src_dir = tmpfs_dir(); // tmpfs → ext4: rename must EXDEV
    let dst_dir = ext4_dir();
    let a = write_file(src_dir.path(), "a.txt", b"aaaa");
    let b = write_file(src_dir.path(), "b.txt", b"bbbb");

    let handle = spawn_job(move_spec(vec![a.clone(), b.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.renames, 0, "no rename may succeed across fs");
    assert_eq!(summary.files_copied, 2);
    assert!(!a.exists() && !b.exists(), "sources deleted after verified copy");
    assert_eq!(std::fs::read(dst_dir.path().join("a.txt")).unwrap(), b"aaaa");
    assert_eq!(std::fs::read(dst_dir.path().join("b.txt")).unwrap(), b"bbbb");
}

#[test]
fn move_failure_preserves_source() {
    let src_dir = tmpfs_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "precious.txt", b"data");

    // Unwritable destination directory: the copy must fail and the source must
    // survive.
    std::fs::set_permissions(dst_dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let handle = spawn_job(move_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);
    std::fs::set_permissions(dst_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    if unsafe { libc_geteuid() } == 0 {
        return; // root ignores permissions; the premise doesn't hold
    }
    assert_eq!(summary.outcome, Outcome::CompletedWithErrors);
    assert!(src.exists(), "failed move must never lose the source");
    assert_eq!(std::fs::read(&src).unwrap(), b"data");
}

// ---- Tier 1 / Tier 2 ------------------------------------------------------

#[test]
fn tier1_fallback_on_ext4() {
    // ext4 has no reflink: FICLONE fails once, is cached, content still arrives.
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let a = write_file(src_dir.path(), "a.bin", &[7u8; 100_000]);
    let b = write_file(src_dir.path(), "b.bin", &[9u8; 50_000]);

    let handle = spawn_job(copy_spec(vec![a, b], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.reflinks, 0);
    assert_eq!(
        summary.stats.copy_file_range + summary.stats.read_write,
        2,
        "both files fell through to Tier 2"
    );
    assert_eq!(
        std::fs::read(dst_dir.path().join("a.bin")).unwrap(),
        vec![7u8; 100_000]
    );
}

#[test]
#[ignore = "needs a btrfs mount: scripts/btrfs-loop.sh, then HOJA_TEST_BTRFS=<mnt>"]
fn tier1_btrfs_reflink_succeeds() {
    let Ok(mnt) = std::env::var("HOJA_TEST_BTRFS") else {
        panic!("set HOJA_TEST_BTRFS to a writable btrfs mount");
    };
    let base = Path::new(&mnt);
    let src_dir = tempfile::tempdir_in(base).unwrap();
    let dst_dir = tempfile::tempdir_in(base).unwrap();
    let a = write_file(src_dir.path(), "a.bin", &[3u8; 1_000_000]);

    let handle = spawn_job(copy_spec(vec![a], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.reflinks, 1, "same-fs btrfs copy must clone");
    assert_eq!(
        std::fs::read(dst_dir.path().join("a.bin")).unwrap(),
        vec![3u8; 1_000_000]
    );
}

#[test]
fn tier2_size_edge_cases() {
    let src_dir = ext4_dir();
    for (dst_name, dst_dir) in [("ext4", ext4_dir()), ("tmpfs", tmpfs_dir())] {
        let buf_size = 4 * 1024 * 1024usize;
        let cases: Vec<(String, Vec<u8>)> = vec![
            ("empty.bin".into(), vec![]),
            ("one.bin".into(), vec![42]),
            ("exact.bin".into(), vec![1u8; buf_size]),
            ("minus.bin".into(), vec![2u8; buf_size - 1]),
            ("plus.bin".into(), vec![3u8; buf_size + 1]),
        ];
        let sources: Vec<_> = cases
            .iter()
            .map(|(name, data)| write_file(src_dir.path(), name, data))
            .collect();

        let handle = spawn_job(copy_spec(sources, dst_dir.path())).unwrap();
        let (_, summary) = drain(&handle, never_conflict);
        assert_eq!(summary.outcome, Outcome::Completed, "dest = {dst_name}");

        for (name, data) in &cases {
            assert_eq!(
                &std::fs::read(dst_dir.path().join(name)).unwrap(),
                data,
                "content mismatch for {name} → {dst_name}"
            );
        }
        for (name, _) in &cases {
            std::fs::remove_file(src_dir.path().join(name)).unwrap();
        }
    }
}

#[test]
fn tier2_sparse_files_stay_sparse() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();

    // 64MB file with three small data extents.
    let src = src_dir.path().join("sparse.img");
    let file = std::fs::File::create(&src).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    use std::os::unix::fs::FileExt;
    file.write_all_at(b"start", 0).unwrap();
    file.write_all_at(b"middle", 32 * 1024 * 1024).unwrap();
    file.write_all_at(b"end", 64 * 1024 * 1024 - 3).unwrap();
    drop(file);

    let src_blocks = std::fs::metadata(&src).unwrap().blocks();

    let handle = spawn_job(copy_spec(vec![src.clone()], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);
    assert_eq!(summary.outcome, Outcome::Completed);

    let dest = dst_dir.path().join("sparse.img");
    let dest_meta = std::fs::metadata(&dest).unwrap();
    assert_eq!(dest_meta.len(), 64 * 1024 * 1024);
    assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());
    assert!(
        dest_meta.blocks() <= src_blocks * 2 + 16,
        "holes not preserved: src {} blocks, dest {} blocks",
        src_blocks,
        dest_meta.blocks()
    );
}

// ---- Correctness layer ----------------------------------------------------

#[test]
fn metadata_is_preserved() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let src = write_file(src_dir.path(), "meta.txt", b"content");
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
    let _ = rustix::fs::setxattr(
        &src,
        "user.pane.test",
        b"value",
        rustix::fs::XattrFlags::empty(),
    );
    let src_mtime = std::fs::metadata(&src).unwrap().mtime();

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);
    assert_eq!(summary.outcome, Outcome::Completed);

    let dest = dst_dir.path().join("meta.txt");
    let meta = std::fs::metadata(&dest).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
    assert!((meta.mtime() - src_mtime).abs() <= 1);

    let mut buf = [0u8; 64];
    if let Ok(n) = rustix::fs::getxattr(&dest, "user.pane.test", &mut buf[..]) {
        assert_eq!(&buf[..n], b"value");
    }
}

#[test]
fn symlinks_copied_as_links() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    write_file(src_dir.path(), "target.txt", b"t");
    std::os::unix::fs::symlink("target.txt", src_dir.path().join("rel.lnk")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/nowhere", src_dir.path().join("dangling.lnk"))
        .unwrap();

    let handle = spawn_job(copy_spec(
        vec![
            src_dir.path().join("rel.lnk"),
            src_dir.path().join("dangling.lnk"),
        ],
        dst_dir.path(),
    ))
    .unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.symlinks, 2);
    assert_eq!(
        std::fs::read_link(dst_dir.path().join("rel.lnk")).unwrap(),
        Path::new("target.txt")
    );
    assert_eq!(
        std::fs::read_link(dst_dir.path().join("dangling.lnk")).unwrap(),
        Path::new("/nonexistent/nowhere")
    );
}

#[test]
fn hardlinks_preserved_within_job() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let first = write_file(&tree, "first.bin", &[5u8; 10_000]);
    std::fs::hard_link(&first, tree.join("second.bin")).unwrap();

    let handle = spawn_job(copy_spec(vec![tree], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.stats.hardlinks, 1);
    let a = std::fs::metadata(dst_dir.path().join("tree/first.bin")).unwrap();
    let b = std::fs::metadata(dst_dir.path().join("tree/second.bin")).unwrap();
    assert_eq!(a.ino(), b.ino(), "dest files must share an inode");
    assert_eq!(a.nlink(), 2);
}

#[test]
fn directory_trees_copy_recursively() {
    let src_dir = ext4_dir();
    let dst_dir = tmpfs_dir();
    let root = src_dir.path().join("root");
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    write_file(&root, "a.txt", b"a");
    write_file(&root.join("sub"), "b.txt", b"b");
    write_file(&root.join("sub/deeper"), "c.txt", b"c");

    let handle = spawn_job(copy_spec(vec![root], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(summary.files_copied, 3);
    assert_eq!(
        std::fs::read(dst_dir.path().join("root/sub/deeper/c.txt")).unwrap(),
        b"c"
    );
}

// ---- Conflicts ------------------------------------------------------------

#[test]
fn conflicts_prompt_once_with_apply_to_all() {
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let mut sources = Vec::new();
    for i in 0..5 {
        let name = format!("f{i}.txt");
        sources.push(write_file(src_dir.path(), &name, b"new"));
        write_file(dst_dir.path(), &name, b"old");
    }

    let handle = spawn_job(copy_spec(sources, dst_dir.path())).unwrap();
    let (events, summary) = drain(&handle, || ConflictDecision::Apply {
        choice: ConflictChoice::Skip,
        apply_to_all: true,
    });

    assert_eq!(conflict_count(&events), 1, "apply-to-all: one prompt for five");
    assert_eq!(summary.files_skipped, 5);
    for i in 0..5 {
        assert_eq!(
            std::fs::read(dst_dir.path().join(format!("f{i}.txt"))).unwrap(),
            b"old"
        );
    }
}

#[test]
fn overwrite_and_keep_both() {
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
    assert_eq!(std::fs::read(dst_dir.path().join("x.tar.gz")).unwrap(), b"new");

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
fn moving_a_directory_onto_an_existing_one_merges() {
    // The directory fast path uses NOREPLACE, so an occupied destination must
    // fall through to a merge rather than failing the job.
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    std::fs::create_dir(src_dir.path().join("tree")).unwrap();
    write_file(&src_dir.path().join("tree"), "fresh.txt", b"f");
    std::fs::create_dir(dst_dir.path().join("tree")).unwrap();
    write_file(&dst_dir.path().join("tree"), "existing.txt", b"e");

    let handle = spawn_job(move_spec(
        vec![src_dir.path().join("tree")],
        dst_dir.path(),
    ))
    .unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::Completed);
    assert!(dst_dir.path().join("tree/existing.txt").exists());
    assert!(dst_dir.path().join("tree/fresh.txt").exists());
}

// ---- Errors, cancellation -------------------------------------------------

#[test]
fn errors_queue_and_job_continues() {
    if unsafe { libc_geteuid() } == 0 {
        return;
    }
    let src_dir = ext4_dir();
    let dst_dir = ext4_dir();
    let tree = src_dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    write_file(&tree, "ok1.txt", b"1");
    let bad = write_file(&tree, "bad.txt", b"x");
    write_file(&tree, "ok2.txt", b"2");
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

    let handle = spawn_job(copy_spec(vec![tree], dst_dir.path())).unwrap();
    let (_, summary) = drain(&handle, never_conflict);

    assert_eq!(summary.outcome, Outcome::CompletedWithErrors);
    assert_eq!(summary.errors.len(), 1);
    assert_eq!(summary.files_copied, 2, "the other files still copied");
    assert!(dst_dir.path().join("tree/ok1.txt").exists());
    assert!(dst_dir.path().join("tree/ok2.txt").exists());
}

#[test]
fn cancel_leaves_no_partials() {
    let src_dir = ext4_dir();
    let dst_dir = tmpfs_dir();
    // Big enough that cancellation lands mid-copy.
    let src = write_file(src_dir.path(), "big.bin", &vec![1u8; 256 * 1024 * 1024]);

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    // Wait until bytes start moving, then cancel.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while handle
        .progress()
        .bytes_done
        .load(std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    handle.cancel();

    let (_, summary) = drain(&handle, never_conflict);
    assert_eq!(summary.outcome, Outcome::Cancelled);
    no_partials_under(dst_dir.path());
}

#[test]
fn atomic_visibility() {
    // The final name must never exist with partial contents. Poll while copying.
    let src_dir = ext4_dir();
    let dst_dir = tmpfs_dir();
    let len = 128 * 1024 * 1024u64;
    let src = write_file(src_dir.path(), "atomic.bin", &vec![9u8; len as usize]);
    let dest = dst_dir.path().join("atomic.bin");

    let handle = spawn_job(copy_spec(vec![src], dst_dir.path())).unwrap();
    loop {
        if let Ok(meta) = std::fs::metadata(&dest) {
            assert_eq!(meta.len(), len, "final name appeared with partial size");
        }
        if handle.is_finished() {
            break;
        }
        std::thread::yield_now();
    }
    let (_, summary) = drain(&handle, never_conflict);
    assert_eq!(summary.outcome, Outcome::Completed);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), len);
}

// ---- Property-style round-trip -------------------------------------------

#[test]
fn random_sparse_files_round_trip() {
    // Deterministic xorshift; no external RNG dependency.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let src_dir = ext4_dir();
    for case in 0..6 {
        // Alternate destinations: ext4 (cfr path) and tmpfs (forced read/write).
        let dst_dir = if case % 2 == 0 { ext4_dir() } else { tmpfs_dir() };
        let len = (next() % (8 * 1024 * 1024)).max(1);
        let src = src_dir.path().join(format!("rand{case}.bin"));
        let file = std::fs::File::create(&src).unwrap();
        file.set_len(len).unwrap();

        use std::os::unix::fs::FileExt;
        let extents = next() % 8;
        for _ in 0..extents {
            let off = next() % len;
            let size = (next() % 64 * 1024).min(len - off).max(1);
            let chunk: Vec<u8> = (0..size).map(|_| next() as u8).collect();
            file.write_all_at(&chunk, off).unwrap();
        }
        drop(file);

        let handle = spawn_job(copy_spec(vec![src.clone()], dst_dir.path())).unwrap();
        let (_, summary) = drain(&handle, never_conflict);
        assert_eq!(summary.outcome, Outcome::Completed, "case {case}");
        assert_eq!(
            std::fs::read(&src).unwrap(),
            std::fs::read(dst_dir.path().join(format!("rand{case}.bin"))).unwrap(),
            "case {case} content mismatch"
        );
        std::fs::remove_file(&src).unwrap();
    }
}

#[test]
fn a_directory_cannot_be_moved_into_itself() {
    let root = ext4_dir();
    let tree = root.path().join("project");
    std::fs::create_dir_all(tree.join("src")).unwrap();
    write_file(&tree.join("src"), "main.rs", b"fn main() {}");

    // Into its own descendant: the rename fast path would fail with EINVAL and
    // the fallback would copy a tree into itself.
    let rejected = |spec| match spawn_job(spec) {
        Ok(_) => panic!("expected the job to be refused"),
        Err(err) => err.kind(),
    };
    assert_eq!(
        rejected(move_spec(vec![tree.clone()], &tree.join("src"))),
        std::io::ErrorKind::InvalidInput
    );

    // Into itself.
    assert_eq!(
        rejected(copy_spec(vec![tree.clone()], &tree)),
        std::io::ErrorKind::InvalidInput
    );

    // Nothing was touched.
    assert!(tree.join("src/main.rs").exists());
    assert_eq!(std::fs::read_dir(tree.join("src")).unwrap().count(), 1);

    // A sibling is still fine, and a shared prefix is not containment.
    let sibling = root.path().join("project-notes");
    std::fs::create_dir(&sibling).unwrap();
    assert!(spawn_job(copy_spec(vec![tree], &sibling)).is_ok());
}
