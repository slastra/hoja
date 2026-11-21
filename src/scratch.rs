//! Working space that belongs to one hoja and is cleaned up after it.
//!
//! Each user of this has a root, which every hoja shares. Under it each
//! process makes one directory, named for its pid, holding a `lock` file that
//! it keeps `flock`ed for as long as it lives. Ownership is the `flock`, not
//! the pid, for the reason `journal.rs` gives: a pid can be recycled, but a
//! lock is released the instant its holder dies, however it dies. A directory
//! whose lock can be taken belongs to nobody, and `sweep` removes it.

use std::fs::File;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{FlockOperation, flock};

pub struct Scratch {
    root: fn() -> Option<PathBuf>,
    /// This process's directory and the lock that says it is alive. Made on
    /// first use, held for the rest of the session.
    own: OnceLock<Option<(PathBuf, File)>>,
    next: AtomicU64,
}

impl Scratch {
    pub const fn new(root: fn() -> Option<PathBuf>) -> Self {
        Scratch {
            root,
            own: OnceLock::new(),
            next: AtomicU64::new(0),
        }
    }

    /// A fresh name in this process's directory, not yet made. Every call
    /// gives a different one.
    pub fn next_path(&self, extension: &str) -> Option<PathBuf> {
        let (dir, _lock) = self.own.get_or_init(|| self.claim()).as_ref()?;
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        Some(dir.join(format!("{n}{extension}")))
    }

    /// A fresh, empty directory in this process's directory.
    pub fn next_dir(&self) -> Option<PathBuf> {
        let dir = self.next_path("")?;
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    fn claim(&self) -> Option<(PathBuf, File)> {
        // What dead hojas left is cleared before this one adds to it.
        self.sweep();
        let dir = (self.root)()?.join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).ok()?;
        let lock = File::create(dir.join("lock")).ok()?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).ok()?;
        Some((dir, lock))
    }

    /// Remove the directories of every hoja that is no longer running.
    pub fn sweep(&self) {
        let Some(root) = (self.root)() else { return };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(lock) = File::open(dir.join("lock")) else {
                // No lock file at all: not one of ours, or one that never got
                // as far as locking. Either way there is no owner to defer to.
                let _ = std::fs::remove_dir_all(&dir);
                continue;
            };
            // A lock held through another open file, this process's own
            // included, refuses this one, so a live directory survives.
            if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Option<PathBuf> {
        Some(std::env::temp_dir().join(format!("hoja-scratch-test-{}", std::process::id())))
    }

    #[test]
    fn the_dead_are_swept_and_the_living_kept() {
        static SCRATCH: Scratch = Scratch::new(root);
        let base = root().unwrap();
        let _ = std::fs::remove_dir_all(&base);

        // A dead hoja's directory: a lock file nobody holds.
        let dead = base.join("1");
        std::fs::create_dir_all(&dead).unwrap();
        File::create(dead.join("lock")).unwrap();
        std::fs::write(dead.join("0.png"), b"left behind").unwrap();

        // Claiming sweeps first.
        let path = SCRATCH.next_path(".png").unwrap();
        assert!(!dead.exists());
        assert_eq!(path.extension().unwrap(), "png");
        assert_ne!(SCRATCH.next_path(".png").unwrap(), path);
        let dir = SCRATCH.next_dir().unwrap();
        assert!(dir.is_dir());

        // This process's own directory survives a sweep.
        SCRATCH.sweep();
        assert!(dir.is_dir());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
