//! Per-pane navigation history: a linear list with a cursor, browser-style.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct History {
    entries: Vec<PathBuf>,
    ix: usize,
}

impl History {
    pub fn new(start: PathBuf) -> Self {
        Self {
            entries: vec![start],
            ix: 0,
        }
    }

    pub fn current(&self) -> &Path {
        &self.entries[self.ix]
    }

    /// Record a navigation: everything forward of the cursor is discarded,
    /// exactly like a browser. Navigating to the current entry is a no-op so
    /// refreshes and redundant clicks don't pollute the list.
    pub fn push(&mut self, dir: PathBuf) {
        if self.entries[self.ix] == dir {
            return;
        }
        self.entries.truncate(self.ix + 1);
        self.entries.push(dir);
        self.ix = self.entries.len() - 1;
    }

    pub fn can_back(&self) -> bool {
        self.ix > 0
    }

    pub fn can_forward(&self) -> bool {
        self.ix + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<&Path> {
        if self.can_back() {
            self.ix -= 1;
            Some(self.current())
        } else {
            None
        }
    }

    pub fn forward(&mut self) -> Option<&Path> {
        if self.can_forward() {
            self.ix += 1;
            Some(self.current())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn push_advances_and_back_forward_walk() {
        let mut h = History::new(p("/a"));
        h.push(p("/a/b"));
        h.push(p("/a/b/c"));

        assert!(h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(p("/a/b").as_path()));
        assert_eq!(h.back(), Some(p("/a").as_path()));
        assert_eq!(h.back(), None, "stops at the beginning");
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(p("/a/b").as_path()));
    }

    #[test]
    fn push_truncates_the_forward_tail() {
        let mut h = History::new(p("/a"));
        h.push(p("/b"));
        h.push(p("/c"));
        h.back();
        h.back(); // at /a, forward tail = [/b, /c]

        h.push(p("/d")); // browser rule: tail discarded
        assert!(!h.can_forward());
        assert_eq!(h.current(), p("/d").as_path());
        assert_eq!(h.back(), Some(p("/a").as_path()));
        assert_eq!(h.forward(), Some(p("/d").as_path()));
    }

    #[test]
    fn pushing_the_current_entry_is_a_noop() {
        let mut h = History::new(p("/a"));
        h.push(p("/b"));
        h.back();
        h.push(p("/a")); // same as current: must not truncate the tail
        assert!(h.can_forward(), "no-op push must preserve the forward tail");
    }
}
