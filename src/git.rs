//! Git status for one directory listing.
//!
//! pane shows status as the colour of the file name, the way Zed's project
//! panel does, rather than as a column. That choice avoids three problems at
//! once: no column is wasted on directories outside a repository, nothing has
//! to reflow when the status arrives late, and a row keeps one line of text.
//!
//! Status comes from `git` itself rather than from a Rust reimplementation.
//! Ignore rules are the whole difficulty here: `.gitignore` at every level,
//! `core.excludesFile`, `info/exclude`, negations, submodules, sparse checkouts,
//! and git is the only thing guaranteed to agree with what the user sees on
//! the command line. The cost is a process spawn, which is why this never runs
//! on the UI thread: measured at 7ms for a subdirectory of a 4k-file repository
//! and 142ms in a 17k-file one with 23 submodules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What happened to a path, in the order a row should prefer when a directory
/// rolls up several children. The order is Zed's (`entry_git_aware_label_color`
/// in its editor crate): a conflict over a deletion over a change over an
/// addition, and ignored below all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitStatus {
    /// Tracked, committed, unmodified. Never stored, the absence of an entry
    /// means this, but directories need it when rolling up.
    Unmodified,
    Ignored,
    Added,
    Renamed,
    Modified,
    Deleted,
    Conflict,
}

impl GitStatus {
    /// Porcelain v2 uses two columns, staged then unstaged, with `.` for "no
    /// change". The more serious of the two wins, which is what makes a file
    /// that is staged *and* then edited read as modified.
    fn from_xy(x: u8, y: u8) -> GitStatus {
        let of = |c: u8| match c {
            b'A' => GitStatus::Added,
            b'R' | b'C' => GitStatus::Renamed,
            b'M' | b'T' => GitStatus::Modified,
            b'D' => GitStatus::Deleted,
            _ => GitStatus::Unmodified,
        };
        of(x).max(of(y))
    }
}

/// Statuses for the entries of one directory, keyed by file name.
///
/// Only names directly in the listed directory appear. A path deeper in the
/// tree rolls up into the directory entry that contains it, so a folder shows
/// that something inside it changed.
pub type GitStatuses = HashMap<String, GitStatus>;

/// The work tree root containing `dir`, or `None` when it is not in a repository.
///
/// `git rev-parse` rather than a walk up looking for `.git`: it costs one spawn
/// and gets worktrees, submodules, and `$GIT_DIR` right, none of which a naive
/// walk does.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    rev_parse(dir, "--show-toplevel")
}

/// The repository's git directory, which a commit, an add, a checkout or a
/// reset writes to and nothing else in the listing shows. Watched for that.
pub fn git_dir(dir: &Path) -> Option<PathBuf> {
    rev_parse(dir, "--absolute-git-dir")
}

fn rev_parse(dir: &Path, what: &str) -> Option<PathBuf> {
    let out = git(dir)?.args(["rev-parse", what]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim_end_matches('\n');
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// `git -C dir`, as a background reader should run it.
///
/// `GIT_OPTIONAL_LOCKS=0` stops `git status` refreshing the index as it
/// goes, which it otherwise does whenever a file's stat data has moved on.
/// That write lands in the git directory hoja watches, which would ask for
/// another status, which would write again.
fn git(dir: &Path) -> Option<Command> {
    let mut command = Command::new("git");
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-C", dir.to_str()?]);
    Some(command)
}

/// Status for everything at or below `dir`, folded onto the entries of `dir`.
///
/// Returns an empty map for a directory outside a repository, so the caller
/// does not need to ask first.
pub fn statuses(dir: &Path) -> GitStatuses {
    let Some(root) = repo_root(dir) else {
        return GitStatuses::new();
    };
    // `-z` because file names may contain newlines, and without it git quotes
    // and escapes them instead. `--no-renames` keeps every record two-column,
    // so a rename cannot desynchronise the NUL-separated stream with its extra
    // path field.
    let Some(mut command) = git(dir) else {
        return GitStatuses::new();
    };
    let out = command
        .args([
            "status",
            "--porcelain=v2",
            "-z",
            "--no-renames",
            "--untracked-files=normal",
            // `traditional` collapses a wholly ignored directory to one record
            // rather than listing what is inside it: `! target/` instead of the
            // 22k files under it. Worth the cost (4ms to 39ms on this repo,
            // nothing measurable on a large one) to dim build output.
            "--ignored=traditional",
            "--",
            ".",
        ])
        .output();
    let Ok(out) = out else {
        return GitStatuses::new();
    };
    if !out.status.success() {
        return GitStatuses::new();
    }

    let mut statuses = GitStatuses::new();
    for record in out.stdout.split(|&b| b == 0) {
        let Some((status, path)) = parse_record(record) else {
            continue;
        };
        // Paths are relative to the work tree root, not to `dir`.
        let full = root.join(path);
        let Ok(rest) = full.strip_prefix(dir) else {
            continue;
        };
        // The first component is the entry in this listing; anything deeper
        // rolls up into it.
        let mut components = rest.components();
        let Some(name) = components.next() else {
            continue;
        };
        // Except being ignored. That is said of the entry itself, as Zed has
        // it: a tracked folder with an ignored log file in it is not an
        // ignored folder, and dimming it would say it was.
        if status == GitStatus::Ignored && components.next().is_some() {
            continue;
        }
        let name = name.as_os_str().to_string_lossy().into_owned();
        statuses
            .entry(name)
            .and_modify(|existing| *existing = (*existing).max(status))
            .or_insert(status);
    }
    statuses
}

/// One NUL-separated porcelain v2 record into a status and a repo-relative path.
///
/// Record forms, by leading character:
///   `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`   a tracked change
///   `u <XY> ...  <path>`                             unmerged, a conflict
///   `? <path>`                                       untracked
///   `! <path>`                                       ignored
///   `# ...`                                          a header, skipped
fn parse_record(record: &[u8]) -> Option<(GitStatus, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;

    let path_at = |field: usize| {
        // Fields are single-space separated; the path is the rest of the record
        // and may itself contain spaces.
        let mut rest = record;
        for _ in 0..field {
            let space = rest.iter().position(|&b| b == b' ')?;
            rest = &rest[space + 1..];
        }
        (!rest.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(rest)))
    };

    match record.first()? {
        b'1' => {
            let xy = record.get(2..4)?;
            Some((GitStatus::from_xy(xy[0], xy[1]), path_at(8)?))
        }
        b'u' => Some((GitStatus::Conflict, path_at(10)?)),
        b'?' => Some((GitStatus::Added, path_at(1)?)),
        b'!' => Some((GitStatus::Ignored, path_at(1)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worse_status_wins_between_the_two_columns() {
        // Staged add, then edited in the work tree: modified is the honest read.
        assert_eq!(GitStatus::from_xy(b'A', b'M'), GitStatus::Modified);
        assert_eq!(GitStatus::from_xy(b'M', b'.'), GitStatus::Modified);
        assert_eq!(GitStatus::from_xy(b'.', b'M'), GitStatus::Modified);
        assert_eq!(GitStatus::from_xy(b'A', b'.'), GitStatus::Added);
        assert_eq!(GitStatus::from_xy(b'.', b'D'), GitStatus::Deleted);
        assert_eq!(GitStatus::from_xy(b'.', b'.'), GitStatus::Unmodified);
    }

    #[test]
    fn parses_each_record_form() {
        let tracked = b"1 .M N... 100644 100644 100644 abc def src/main.rs";
        assert_eq!(
            parse_record(tracked),
            Some((GitStatus::Modified, PathBuf::from("src/main.rs")))
        );

        let untracked = b"? notes.txt";
        assert_eq!(
            parse_record(untracked),
            Some((GitStatus::Added, PathBuf::from("notes.txt")))
        );

        let ignored = b"! target";
        assert_eq!(
            parse_record(ignored),
            Some((GitStatus::Ignored, PathBuf::from("target")))
        );

        // Headers carry no path.
        assert_eq!(parse_record(b"# branch.oid abc123"), None);
        assert_eq!(parse_record(b""), None);
    }

    /// A real repository: what `git status` says, folded onto a listing.
    #[test]
    fn statuses_fold_onto_the_listing_as_zed_shows_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-C", root.to_str().unwrap()])
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("clean")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\ntarget/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("clean/a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);

        // A tracked folder with an ignored file inside, and nothing else.
        std::fs::write(root.join("clean/debug.log"), "x\n").unwrap();
        // A change deep in a folder, an untracked file, an ignored folder.
        std::fs::write(root.join("src/main.rs"), "fn main() { }\n").unwrap();
        std::fs::write(root.join("new.txt"), "n\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out"), "o\n").unwrap();

        let statuses = statuses(root);
        assert_eq!(statuses.get("src"), Some(&GitStatus::Modified));
        assert_eq!(statuses.get("new.txt"), Some(&GitStatus::Added));
        assert_eq!(statuses.get("target"), Some(&GitStatus::Ignored));
        assert_eq!(
            statuses.get("clean"),
            None,
            "an ignored file does not dim the tracked folder it is in"
        );
        assert!(git_dir(root).is_some_and(|d| d.ends_with(".git")));
    }

    #[test]
    fn a_path_may_contain_spaces() {
        let record = b"1 .M N... 100644 100644 100644 abc def my notes/a file.txt";
        assert_eq!(
            parse_record(record),
            Some((GitStatus::Modified, PathBuf::from("my notes/a file.txt")))
        );
        assert_eq!(
            parse_record(b"? a file.txt"),
            Some((GitStatus::Added, PathBuf::from("a file.txt")))
        );
    }
}
