//! Per-pane navigation history: a linear list with a cursor, browser-style.
//!
//! Holds `Location` rather than `PathBuf`, so stepping into an archive and
//! back out again is one list and not two.

use std::path::PathBuf;

use crate::location::Location;

/// Where someone was in a listing when they left it: what a browser keeps for
/// a page so that Back returns to the same spot rather than to the top.
///
/// By row key rather than by index, so a listing that changed while it was
/// away degrades gracefully: a row that is gone is simply not found, and the
/// rest still is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    pub cursor: Option<PathBuf>,
    pub selected: Vec<PathBuf>,
    /// The first row that was in view.
    pub top: Option<PathBuf>,
}

#[derive(Debug)]
struct Entry {
    dir: Location,
    /// `None` until the entry is left for the first time.
    place: Option<Place>,
}

#[derive(Debug)]
pub struct History {
    entries: Vec<Entry>,
    ix: usize,
}

impl History {
    pub fn new(start: Location) -> Self {
        Self {
            entries: vec![Entry {
                dir: start,
                place: None,
            }],
            ix: 0,
        }
    }

    /// Record a navigation: everything forward of the cursor is discarded,
    /// exactly like a browser. Navigating to the current entry is a no-op so
    /// refreshes and redundant clicks don't pollute the list.
    ///
    /// `leaving` is where the listing being left was, kept on its entry for
    /// when Back returns to it.
    pub fn push(&mut self, dir: Location, leaving: Place) {
        if self.entries[self.ix].dir == dir {
            return;
        }
        self.entries[self.ix].place = Some(leaving);
        self.entries.truncate(self.ix + 1);
        self.entries.push(Entry { dir, place: None });
        self.ix = self.entries.len() - 1;
    }

    pub fn can_back(&self) -> bool {
        self.ix > 0
    }

    pub fn can_forward(&self) -> bool {
        self.ix + 1 < self.entries.len()
    }

    /// Step back, keeping `leaving` on the entry stepped away from. Returns
    /// where to go and where in it to stand.
    pub fn back(&mut self, leaving: Place) -> Option<(&Location, Option<&Place>)> {
        if !self.can_back() {
            return None;
        }
        self.entries[self.ix].place = Some(leaving);
        self.ix -= 1;
        Some(self.here())
    }

    /// Step forward, the same way.
    pub fn forward(&mut self, leaving: Place) -> Option<(&Location, Option<&Place>)> {
        if !self.can_forward() {
            return None;
        }
        self.entries[self.ix].place = Some(leaving);
        self.ix += 1;
        Some(self.here())
    }

    fn here(&self) -> (&Location, Option<&Place>) {
        let entry = &self.entries[self.ix];
        (&entry.dir, entry.place.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Location {
        Location::Disk(std::path::PathBuf::from(s))
    }

    /// Step without a place to leave, for the tests that are about the list.
    fn back(h: &mut History) -> Option<Location> {
        h.back(Place::default()).map(|(dir, _)| dir.clone())
    }

    fn forward(h: &mut History) -> Option<Location> {
        h.forward(Place::default()).map(|(dir, _)| dir.clone())
    }

    #[test]
    fn push_advances_and_back_forward_walk() {
        let mut h = History::new(p("/a"));
        h.push(p("/a/b"), Place::default());
        h.push(p("/a/b/c"), Place::default());

        assert!(h.can_back());
        assert!(!h.can_forward());
        assert_eq!(back(&mut h), Some(p("/a/b")));
        assert_eq!(back(&mut h), Some(p("/a")));
        assert_eq!(back(&mut h), None, "stops at the beginning");
        assert!(h.can_forward());
        assert_eq!(forward(&mut h), Some(p("/a/b")));
    }

    #[test]
    fn push_truncates_the_forward_tail() {
        let mut h = History::new(p("/a"));
        h.push(p("/b"), Place::default());
        h.push(p("/c"), Place::default());
        back(&mut h);
        back(&mut h); // at /a, forward tail = [/b, /c]

        h.push(p("/d"), Place::default()); // browser rule: tail discarded
        assert!(!h.can_forward());
        assert_eq!(h.here().0, &p("/d"));
        assert_eq!(back(&mut h), Some(p("/a")));
        assert_eq!(forward(&mut h), Some(p("/d")));
    }

    #[test]
    fn pushing_the_current_entry_is_a_noop() {
        let mut h = History::new(p("/a"));
        h.push(p("/b"), Place::default());
        back(&mut h);
        h.push(p("/a"), Place::default()); // same as current: must not truncate the tail
        assert!(h.can_forward(), "no-op push must preserve the forward tail");
    }

    #[test]
    fn back_and_forward_return_to_where_each_listing_was_left() {
        let at = |name: &str| Place {
            cursor: Some(std::path::PathBuf::from(name)),
            selected: vec![std::path::PathBuf::from(name)],
            top: Some(std::path::PathBuf::from("first")),
        };
        let mut h = History::new(p("/a"));
        // Left /a from the folder that was opened, as a double-click does.
        h.push(p("/a/b"), at("/a/b"));
        let (dir, place) = h.back(at("/a/b/x")).unwrap();
        assert_eq!(dir, &p("/a"));
        assert_eq!(place, Some(&at("/a/b")), "back lands on the folder entered");
        let (dir, place) = h.forward(at("/a/b")).unwrap();
        assert_eq!(dir, &p("/a/b"));
        assert_eq!(
            place,
            Some(&at("/a/b/x")),
            "and forward on where it was left"
        );
        // A fresh navigation has no place yet: it opens at the top.
        h.push(p("/a/b/c"), at("/a/b/c"));
        assert_eq!(h.here().1, None);
    }

    #[test]
    fn an_archive_shares_the_one_list() {
        // Stepping in and back out again is ordinary navigation, not a mode.
        let zip = std::path::PathBuf::from("/a/pack.zip");
        let mut h = History::new(p("/a"));
        h.push(Location::in_archive(zip.clone()), Place::default());
        h.push(
            Location::Archive {
                archive: zip.clone(),
                inside: std::path::PathBuf::from("ttf"),
            },
            Place::default(),
        );

        assert_eq!(back(&mut h), Some(Location::in_archive(zip)));
        assert_eq!(back(&mut h), Some(p("/a")));
    }
}
