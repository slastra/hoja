//! Recursive name search under one directory.
//!
//! A walk, not an index. `plocate` is installed on this machine and is the
//! obvious shortcut, but its database is rebuilt nightly, so nothing made today
//! is in it — measured: no file of this project appears, because the project is
//! a day old, and "where did I just save that" is the search people actually
//! run. `PRUNEPATHS` also excludes /media and /mnt and `PRUNEFS` excludes
//! sshfs, nfs and cifs, so a removable drive would never be found at all.
//!
//! Walking is fast enough that the index buys nothing at this scope: `fd` over
//! ~/Projects, 2.5 million files, takes 0.39s warm. Results stream, so the
//! first hits appear immediately regardless.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

use crate::fs::{self, DirEntry};

/// Stop before a runaway query — a single letter under a home directory matches
/// hundreds of thousands of files, and neither the list nor the memory is worth
/// spending on results nobody will scroll to.
pub const RESULT_CAP: usize = 2000;

pub struct Search {
    results: Receiver<Vec<DirEntry>>,
    cancel: Arc<AtomicBool>,
    /// Set once the walk has finished or hit the cap.
    done: Arc<AtomicBool>,
    capped: Arc<AtomicBool>,
}

impl Search {
    /// Take whatever has arrived since the last call.
    pub fn drain(&self) -> Vec<DirEntry> {
        let mut batch = Vec::new();
        while let Ok(mut more) = self.results.try_recv() {
            batch.append(&mut more);
        }
        batch
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn hit_cap(&self) -> bool {
        self.capped.load(Ordering::Relaxed)
    }
}

impl Drop for Search {
    /// Dropping the handle stops the walk, so a new keystroke abandons the
    /// previous query rather than racing it.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Walk `root` for names matching `query`, breadth-first, streaming batches.
///
/// Breadth-first so shallow matches — nearly always the wanted ones — arrive
/// before the walker disappears into a deep build directory.
pub fn spawn(root: PathBuf, query: String, show_hidden: bool) -> Search {
    let (tx, results) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let capped = Arc::new(AtomicBool::new(false));

    std::thread::Builder::new()
        .name("hoja-search".into())
        .spawn({
            let cancel = cancel.clone();
            let done = done.clone();
            let capped = capped.clone();
            move || {
                let mut queue = std::collections::VecDeque::from([root.clone()]);
                let mut batch: Vec<DirEntry> = Vec::new();
                let mut found = 0usize;

                while let Some(dir) = queue.pop_front() {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(reader) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in reader.flatten() {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let raw = entry.file_name();
                        let name = raw.to_string_lossy();
                        if name.starts_with('.') && !show_hidden {
                            continue;
                        }
                        if hoja_transfer::is_partial_name(&name) {
                            continue;
                        }

                        // `file_type` comes from the readdir record on Linux;
                        // `metadata` is a statx per entry. Only the recursion
                        // decision is needed for the whole tree, and only the
                        // handful of hits need size and time — asking for
                        // metadata up front cost 8.9s over 2.5M entries against
                        // 1.7s for this.
                        let is_dir = entry
                            .file_type()
                            .map(|t| {
                                t.is_dir()
                                    || (t.is_symlink()
                                        && entry.path().metadata().is_ok_and(|m| m.is_dir()))
                            })
                            .unwrap_or(false);
                        if is_dir {
                            queue.push_back(entry.path());
                        }
                        if !fs::matches_filter(&name, &query) {
                            continue;
                        }

                        let mut item = DirEntry::from_std(&entry);
                        // Show where it is, not just what it is called: a bare
                        // name is ambiguous once results span the tree. This is
                        // a label, so it goes beside the name rather than over
                        // it — rename and git colouring key on `name`.
                        item.relative = Some(
                            item.path
                                .strip_prefix(&root)
                                .unwrap_or(&item.path)
                                .display()
                                .to_string(),
                        );
                        batch.push(item);
                        found += 1;
                        if found >= RESULT_CAP {
                            capped.store(true, Ordering::Relaxed);
                            break;
                        }
                    }

                    // Send per directory: enough to feel immediate without a
                    // channel message per file.
                    if !batch.is_empty() && tx.send(std::mem::take(&mut batch)).is_err() {
                        break;
                    }
                    if capped.load(Ordering::Relaxed) {
                        break;
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(batch);
                }
                done.store(true, Ordering::Relaxed);
            }
        })
        .ok();

    Search {
        results,
        cancel,
        done,
        capped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(search: &Search) -> Vec<String> {
        let mut found = Vec::new();
        for _ in 0..200 {
            found.extend(search.drain().into_iter().filter_map(|e| e.relative));
            if search.is_done() {
                found.extend(search.drain().into_iter().filter_map(|e| e.relative));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        found.sort();
        found
    }

    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hoja-search-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/deep/deeper")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("top-note.txt"), "x").unwrap();
        std::fs::write(root.join("a/mid-note.txt"), "x").unwrap();
        std::fs::write(root.join("a/deep/deeper/buried-note.txt"), "x").unwrap();
        std::fs::write(root.join("a/unrelated.md"), "x").unwrap();
        std::fs::write(root.join(".hidden/secret-note.txt"), "x").unwrap();
        root
    }

    #[test]
    fn finds_matches_at_every_depth_named_by_relative_path() {
        let root = fixture("depth");
        let found = wait(&spawn(root.clone(), "note".into(), false));

        assert_eq!(
            found,
            vec![
                "a/deep/deeper/buried-note.txt",
                "a/mid-note.txt",
                "top-note.txt",
            ],
            "results are named relative to the search root"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hidden_entries_follow_the_pane_setting() {
        let root = fixture("hidden");
        assert!(
            !wait(&spawn(root.clone(), "secret".into(), false)).iter().any(|n| n.contains("secret")),
            "a hidden directory is not searched unless hidden files are shown"
        );
        assert_eq!(
            wait(&spawn(root.clone(), "secret".into(), true)),
            vec![".hidden/secret-note.txt"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn matching_is_smart_cased_and_finds_directories_too() {
        let root = fixture("case");
        assert_eq!(wait(&spawn(root.clone(), "DEEPER".into(), false)), Vec::<String>::new());
        assert_eq!(wait(&spawn(root.clone(), "deeper".into(), false)), vec!["a/deep/deeper"]);
        let _ = std::fs::remove_dir_all(root);
    }
}
