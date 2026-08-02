//! Recursive byte totals for the pane footer, walked in the background.
//!
//! A directory has no size until something counts it, and the footer promises a
//! real one — for a selected folder and for the folder a pane is showing. That
//! is a full tree walk, so it happens off the UI thread, reports as it goes, and
//! stops the moment nobody wants the answer any more.
//!
//! **Walked in parallel**, which is the whole of why `dua` and `dust` are quick:
//! the same stat-every-file work spread across cores. Measured here on a
//! 1,507,072-file tree, warm: 4.16s on one thread, 2.11s on two, 1.09s on four,
//! 1.02s on eight, and 1.11s on thirty-two. Four workers gets essentially all of
//! it, and oversubscribing costs — the same shape the transfer benchmarks
//! showed. `du`, sequential and in C, takes 2.48s.
//!
//! Shaped after `crate::search`, which is the house pattern for a cancellable
//! background walk: a named thread, an `AtomicBool` the walk checks, and a
//! `Drop` that sets it. Atomics rather than that module's channel, because a
//! footer wants one running total rather than a stream of items.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Threads in the walking pool. See the module comment for the measurements.
const WORKERS: usize = 4;

/// A running total. Dropping it stops the walk.
pub struct Measure {
    bytes: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Measure {
    /// Bytes counted so far. Climbs until `is_done`.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl Drop for Measure {
    /// Dropping the handle abandons the walk, so changing the selection or
    /// leaving the directory does not leave a tree walk running for an answer
    /// nothing will read.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Everything the workers share.
struct Walk {
    bytes: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    /// `(dev, ino)` of files with more than one link, so a tree that hardlinks
    /// into a store — pnpm's `node_modules` is 83% such files — is not counted
    /// several times over. Only consulted when `nlink > 1`, so the ordinary
    /// case never takes the lock.
    linked: Mutex<HashSet<(u64, u64)>>,
}

/// Total the bytes under every root, recursively.
///
/// Roots are expected to be directories; a plain file among them is counted and
/// not descended into, so a caller need not sort them first.
pub fn spawn(roots: Vec<PathBuf>) -> Measure {
    let bytes = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    let handle = Measure {
        bytes: bytes.clone(),
        done: done.clone(),
        cancel: cancel.clone(),
    };

    // A thread of our own: `scope` blocks until every task under it finishes,
    // and this must not block a caller.
    let spawned = std::thread::Builder::new()
        .name("hoja-measure".into())
        .spawn(move || {
            let walk = Walk {
                bytes,
                cancel,
                linked: Mutex::new(HashSet::new()),
            };
            // The dedicated pool if it exists, rayon's global one otherwise.
            // The global pool sizes itself to every core, which measured slower
            // than four — but a total that is merely slower beats none at all.
            //
            // Written out twice rather than hoisted into a closure: binding it
            // pins one `'scope` lifetime, and `walk` then fails to outlive it.
            match pool() {
                Some(pool) => pool.scope(|scope| {
                    for root in &roots {
                        enter(scope, root.clone(), &walk);
                    }
                }),
                None => rayon::scope(|scope| {
                    for root in &roots {
                        enter(scope, root.clone(), &walk);
                    }
                }),
            }
            done.store(true, Ordering::Relaxed);
        });

    if spawned.is_err() {
        // No thread, no walk, and no waiting for one.
        handle.done.store(true, Ordering::Relaxed);
    }
    handle
}

fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(WORKERS)
            .thread_name(|i| format!("hoja-measure-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// Count one root, whatever it turns out to be.
fn enter<'a>(scope: &rayon::Scope<'a>, path: PathBuf, walk: &'a Walk) {
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return;
    };
    if meta.is_dir() {
        descend(scope, path, walk);
    } else if meta.is_file() {
        count(&meta, walk);
    }
}

/// Read one directory, counting its files and handing its subdirectories to the
/// pool. Recursion happens through `scope.spawn`, so depth costs stack on the
/// worker rather than on any one thread.
fn descend<'a>(scope: &rayon::Scope<'a>, dir: PathBuf, walk: &'a Walk) {
    // Once per directory, not once per file: a directory is the unit of work,
    // and an atomic load for each of 1.5 million files is not free.
    if walk.cancel.load(Ordering::Relaxed) {
        return;
    }
    let Ok(reader) = std::fs::read_dir(&dir) else {
        // An unreadable subtree contributes nothing rather than failing the
        // whole total. The footer is a convenience; a permission error deeper
        // in the tree is not worth refusing to answer over.
        return;
    };

    for entry in reader.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never followed. A link that points at an ancestor makes the walk a
        // cycle, and `~/.wine/dosdevices/z: -> /` puts one in a great many home
        // directories — `crate::search` learned this the same way. The link
        // itself occupies no space worth reporting.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let path = entry.path();
            scope.spawn(move |scope| descend(scope, path, walk));
        } else if file_type.is_file() {
            // `DirEntry::metadata` does not traverse a symlink, and this is not
            // one anyway.
            if let Ok(meta) = entry.metadata() {
                count(&meta, walk);
            }
        }
    }
}

fn count(meta: &std::fs::Metadata, walk: &Walk) {
    if meta.nlink() > 1 {
        let key = (meta.dev(), meta.ino());
        let mut linked = match walk.linked.lock() {
            Ok(linked) => linked,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Already counted under another name.
        if !linked.insert(key) {
            return;
        }
    }
    walk.bytes.fetch_add(meta.len(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn settle(measure: &Measure) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !measure.is_done() {
            assert!(Instant::now() < deadline, "walk did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn fixture(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("hoja-measure-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        std::fs::write(root.join("top.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(root.join("a/mid.bin"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("a/deep/low.bin"), vec![0u8; 4000]).unwrap();
        root
    }

    #[test]
    fn totals_every_file_below_the_root() {
        let root = fixture("total");
        let measure = spawn(vec![root.clone()]);
        settle(&measure);
        assert_eq!(measure.bytes(), 7000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn several_roots_add_up() {
        let root = fixture("roots");
        let measure = spawn(vec![root.join("a/deep"), root.join("top.bin")]);
        settle(&measure);
        assert_eq!(measure.bytes(), 5000, "the deep tree plus the loose file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlink_cycle_terminates_and_is_not_counted() {
        // The link points back at the root. Following it would never finish;
        // counting its target would count the tree twice.
        let root = fixture("cycle");
        std::os::unix::fs::symlink(&root, root.join("a/loop")).unwrap();

        let measure = spawn(vec![root.clone()]);
        settle(&measure);
        assert_eq!(measure.bytes(), 7000, "counted once, and the walk ended");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hardlinked_file_counts_once() {
        let root = fixture("links");
        // Three names, one inode, 4000 bytes between them — not 12000.
        std::fs::hard_link(root.join("a/deep/low.bin"), root.join("first.bin")).unwrap();
        std::fs::hard_link(root.join("a/deep/low.bin"), root.join("a/second.bin")).unwrap();

        let measure = spawn(vec![root.clone()]);
        settle(&measure);
        assert_eq!(measure.bytes(), 7000, "the extra links add nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dropping_the_handle_stops_the_walk() {
        // Wide enough that the walk cannot plausibly finish before the drop.
        let root =
            std::env::temp_dir().join(format!("hoja-measure-{}-cancel", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in 0..40 {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..200 {
                std::fs::write(dir.join(format!("f{f}")), vec![0u8; 512]).unwrap();
            }
        }

        let cancel = {
            let measure = spawn(vec![root.clone()]);
            // The flag outlives the handle, which is what lets this observe the
            // effect of the drop.
            let cancel = measure.cancel.clone();
            assert!(!cancel.load(Ordering::Relaxed));
            drop(measure);
            cancel
        };
        assert!(cancel.load(Ordering::Relaxed), "the drop asked it to stop");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_directory_does_not_fail_the_total() {
        let root = fixture("unreadable");
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("hidden.bin"), vec![0u8; 8000]).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let measure = spawn(vec![root.clone()]);
        settle(&measure);
        // Running as root would read it anyway, so accept either — the point of
        // the test is that the walk finishes and reports rather than giving up.
        let bytes = measure.bytes();
        assert!(bytes == 7000 || bytes == 15000, "got {bytes}");

        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&root);
    }
}
