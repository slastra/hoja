//! Per-pane navigation history: a linear list with a cursor, browser-style.
//!
//! Holds `Location` rather than `PathBuf`, so stepping into an archive and
//! back out again is one list and not two.

use crate::location::Location;

#[derive(Debug)]
pub struct History {
    entries: Vec<Location>,
    ix: usize,
}

impl History {
    pub fn new(start: Location) -> Self {
        Self {
            entries: vec![start],
            ix: 0,
        }
    }

    pub fn current(&self) -> &Location {
        &self.entries[self.ix]
    }

    /// Record a navigation: everything forward of the cursor is discarded,
    /// exactly like a browser. Navigating to the current entry is a no-op so
    /// refreshes and redundant clicks don't pollute the list.
    pub fn push(&mut self, dir: Location) {
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

    pub fn back(&mut self) -> Option<&Location> {
        if self.can_back() {
            self.ix -= 1;
            Some(self.current())
        } else {
            None
        }
    }

    pub fn forward(&mut self) -> Option<&Location> {
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

    fn p(s: &str) -> Location {
        Location::Disk(std::path::PathBuf::from(s))
    }

    #[test]
    fn push_advances_and_back_forward_walk() {
        let mut h = History::new(p("/a"));
        h.push(p("/a/b"));
        h.push(p("/a/b/c"));

        assert!(h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(&p("/a/b")));
        assert_eq!(h.back(), Some(&p("/a")));
        assert_eq!(h.back(), None, "stops at the beginning");
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(&p("/a/b")));
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
        assert_eq!(h.current(), &p("/d"));
        assert_eq!(h.back(), Some(&p("/a")));
        assert_eq!(h.forward(), Some(&p("/d")));
    }

    #[test]
    fn pushing_the_current_entry_is_a_noop() {
        let mut h = History::new(p("/a"));
        h.push(p("/b"));
        h.back();
        h.push(p("/a")); // same as current: must not truncate the tail
        assert!(h.can_forward(), "no-op push must preserve the forward tail");
    }

    #[test]
    fn an_archive_shares_the_one_list() {
        // Stepping in and back out again is ordinary navigation, not a mode.
        let zip = std::path::PathBuf::from("/a/pack.zip");
        let mut h = History::new(p("/a"));
        h.push(Location::in_archive(zip.clone()));
        h.push(Location::in_archive(zip.clone()).join("ttf"));

        assert_eq!(h.back(), Some(&Location::in_archive(zip)));
        assert_eq!(h.back(), Some(&p("/a")));
    }
}
