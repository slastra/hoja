//! Trash round trips against a real filesystem.
//!
//! These drive the home-trash path by pointing `XDG_DATA_HOME` at a temp dir,
//! which also keeps the developer's real trash out of the test run. Because
//! that is process-global state, every test here shares one lock and one trash
//! directory rather than racing on the environment.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pane_transfer::{TrashedItem, restore, trash};

/// Serialises the tests and points `XDG_DATA_HOME` somewhere disposable.
fn trash_env() -> MutexGuard<'static, PathBuf> {
    static ENV: OnceLock<Mutex<PathBuf>> = OnceLock::new();
    ENV.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("pane-trash-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Safety: set once, before any test body runs, under the lock that
        // every test in this file takes.
        unsafe { std::env::set_var("XDG_DATA_HOME", &root) };
        Mutex::new(root)
    })
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn trash_dir(root: &Path) -> PathBuf {
    root.join("Trash")
}

/// A work directory on the same filesystem as the trash, so deletes are renames.
fn workdir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("work").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

fn info_body(item: &TrashedItem) -> String {
    std::fs::read_to_string(&item.info).unwrap()
}

#[test]
fn trashing_moves_the_file_and_writes_a_sidecar() {
    let root = trash_env();
    let dir = workdir(&root, "basic");
    let file = dir.join("notes.txt");
    write(&file, "hello");

    let item = trash(&file).unwrap();

    assert!(!file.exists(), "original must be gone");
    assert_eq!(std::fs::read_to_string(&item.file).unwrap(), "hello");
    assert_eq!(item.file, trash_dir(&root).join("files/notes.txt"));
    assert_eq!(item.original, file);

    let body = info_body(&item);
    assert!(body.starts_with("[Trash Info]\n"), "body was {body:?}");
    assert!(
        body.contains(&format!("Path={}\n", file.display())),
        "home trash records an absolute path; body was {body:?}"
    );
    // DeletionDate=YYYY-MM-DDThh:mm:ss, no zone suffix.
    let date = body
        .lines()
        .find_map(|l| l.strip_prefix("DeletionDate="))
        .expect("no DeletionDate");
    assert_eq!(date.len(), 19, "unexpected date {date:?}");
    assert_eq!(&date[4..5], "-");
    assert_eq!(&date[10..11], "T");
}

#[test]
fn restore_puts_it_back_and_clears_the_sidecar() {
    let root = trash_env();
    let dir = workdir(&root, "restore");
    let file = dir.join("doc.md");
    write(&file, "body");

    let item = trash(&file).unwrap();
    restore(&item).unwrap();

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "body");
    assert!(!item.file.exists());
    assert!(!item.info.exists(), "sidecar must not outlive the entry");
}

#[test]
fn restore_recreates_a_parent_that_was_deleted_underneath_it() {
    let root = trash_env();
    let dir = workdir(&root, "reparent");
    let nested = dir.join("sub");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("f.txt");
    write(&file, "x");

    let item = trash(&file).unwrap();
    std::fs::remove_dir(&nested).unwrap();
    restore(&item).unwrap();

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
}

#[test]
fn restore_refuses_to_clobber_a_replacement() {
    let root = trash_env();
    let dir = workdir(&root, "clobber");
    let file = dir.join("same.txt");
    write(&file, "original");

    let item = trash(&file).unwrap();
    write(&file, "a newer file with the same name");

    let err = restore(&item).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "a newer file with the same name",
        "the replacement must survive"
    );
    assert!(item.file.exists(), "the trashed copy must survive too");
}

#[test]
fn same_name_from_two_places_gets_a_counter() {
    let root = trash_env();
    let dir = workdir(&root, "collide");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    write(&a.join("dup.txt"), "first");
    write(&b.join("dup.txt"), "second");

    let first = trash(&a.join("dup.txt")).unwrap();
    let second = trash(&b.join("dup.txt")).unwrap();

    assert_eq!(first.file.file_name().unwrap(), "dup.txt");
    assert_eq!(second.file.file_name().unwrap(), "dup.2.txt");
    assert_eq!(std::fs::read_to_string(&first.file).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second.file).unwrap(), "second");
    // Each still knows its own origin, so undo cannot cross them over.
    restore(&second).unwrap();
    restore(&first).unwrap();
    assert_eq!(std::fs::read_to_string(a.join("dup.txt")).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(b.join("dup.txt")).unwrap(), "second");
}

#[test]
fn directories_go_whole() {
    let root = trash_env();
    let dir = workdir(&root, "tree");
    let tree = dir.join("project");
    std::fs::create_dir_all(tree.join("src")).unwrap();
    write(&tree.join("src/main.rs"), "fn main() {}");

    let item = trash(&tree).unwrap();

    assert!(!tree.exists());
    assert_eq!(
        std::fs::read_to_string(item.file.join("src/main.rs")).unwrap(),
        "fn main() {}"
    );
    restore(&item).unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.join("src/main.rs")).unwrap(),
        "fn main() {}"
    );
}

#[test]
fn a_missing_file_is_an_error_and_leaves_no_sidecar() {
    let root = trash_env();
    let dir = workdir(&root, "missing");
    // The trash directory is shared with the other tests, so count the change
    // rather than the total. It may not exist at all if this test runs first,
    // which is itself correct: a missing file must not provoke its creation.
    let sidecars = || {
        std::fs::read_dir(trash_dir(&root).join("info"))
            .map(Iterator::count)
            .unwrap_or(0)
    };
    let before = sidecars();

    let err = trash(&dir.join("nope.txt")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        sidecars(),
        before,
        "a failed trash must not leave a claim behind"
    );
}

#[test]
fn a_relative_path_is_rejected() {
    let _root = trash_env();
    let err = trash(Path::new("relative.txt")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn a_symlink_is_trashed_without_following_it() {
    let root = trash_env();
    let dir = workdir(&root, "symlink");
    let target = dir.join("target.txt");
    write(&target, "payload");
    let link = dir.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let item = trash(&link).unwrap();

    assert!(!link.exists());
    assert!(target.exists(), "the link's target must be untouched");
    assert!(item.file.symlink_metadata().unwrap().file_type().is_symlink());
}
