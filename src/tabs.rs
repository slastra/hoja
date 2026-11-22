//! The tabs of one pane: several locations in one tile of the split tree,
//! one of them shown. Zed's model, in which each pane holds a list of items.
//!
//! This holds only the list and which item is shown. The workspace makes
//! every change, because a change of tab is also a change of focus, of the
//! active pane and of which panes watch their folders.

use gpui::{Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div};

use crate::dir_pane::DirPane;

/// The list itself, with no gpui in it, so its rules can be tested alone.
#[derive(Debug, Clone)]
pub struct TabList<T> {
    items: Vec<T>,
    active: usize,
}

impl<T: PartialEq + Clone> TabList<T> {
    pub fn new(first: T) -> Self {
        Self {
            items: vec![first],
            active: 0,
        }
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn shown(&self) -> &T {
        &self.items[self.active]
    }

    pub fn index_of(&self, item: &T) -> Option<usize> {
        self.items.iter().position(|it| it == item)
    }

    /// Put `item` after the shown tab, as a browser opens a new tab, and
    /// give its index. The shown tab does not change.
    pub fn insert_after_active(&mut self, item: T) -> usize {
        let at = self.active + 1;
        self.items.insert(at, item);
        at
    }

    /// Remove `item`. Refused (false) when it is the last tab, because a pane
    /// with no tabs is a pane to close, which is the caller's to do.
    ///
    /// When the shown tab goes, its right neighbor takes its place, or its left
    /// one when it was the last: the tab that moves into the gap, as in Zed.
    pub fn remove(&mut self, item: &T) -> bool {
        if self.items.len() < 2 {
            return false;
        }
        let Some(at) = self.index_of(item) else {
            return false;
        };
        self.items.remove(at);
        if at < self.active || self.active == self.items.len() {
            self.active -= 1;
        }
        true
    }

    /// Show the tab at `ix`, or the last one when there are fewer.
    pub fn activate(&mut self, ix: usize) {
        self.active = ix.min(self.items.len() - 1);
    }

    /// The index `delta` tabs along, wrapping at both ends.
    pub fn step(&self, delta: isize) -> usize {
        (self.active as isize + delta).rem_euclid(self.items.len() as isize) as usize
    }
}

/// The tabs as a gpui entity: what the split tree holds in each leaf.
pub struct Tabs {
    pub list: TabList<Entity<DirPane>>,
}

impl Tabs {
    pub fn new(first: Entity<DirPane>) -> Self {
        Self {
            list: TabList::new(first),
        }
    }

    pub fn shown(&self) -> &Entity<DirPane> {
        self.list.shown()
    }
}

impl Render for Tabs {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Cached as the tree caches a leaf, so a tab re-renders only when it
        // notifies, not each time the list around it does.
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.shown().clone().cached(crate::pane_group::fill_style()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[u32], active: usize) -> TabList<u32> {
        let mut list = TabList::new(items[0]);
        for item in &items[1..] {
            list.items.push(*item);
        }
        list.active = active;
        list
    }

    #[test]
    fn a_new_tab_goes_after_the_shown_one() {
        let mut tabs = list(&[1, 2, 3], 0);
        assert_eq!(tabs.insert_after_active(9), 1);
        assert_eq!(tabs.items(), &[1, 9, 2, 3]);
        assert_eq!(*tabs.shown(), 1);
    }

    #[test]
    fn closing_the_shown_tab_shows_the_one_that_moves_into_its_place() {
        let mut tabs = list(&[1, 2, 3], 1);
        assert!(tabs.remove(&2));
        assert_eq!(*tabs.shown(), 3);

        // At the end there is nothing on the right, so the left one.
        let mut tabs = list(&[1, 2, 3], 2);
        assert!(tabs.remove(&3));
        assert_eq!(*tabs.shown(), 2);
    }

    #[test]
    fn closing_another_tab_keeps_the_shown_one() {
        let mut tabs = list(&[1, 2, 3], 2);
        assert!(tabs.remove(&1));
        assert_eq!(*tabs.shown(), 3);
        assert!(tabs.remove(&2));
        assert_eq!(*tabs.shown(), 3);
    }

    #[test]
    fn the_last_tab_is_not_removed() {
        let mut tabs = list(&[1], 0);
        assert!(!tabs.remove(&1));
        assert_eq!(tabs.items(), &[1]);
        assert!(!list(&[1, 2], 0).remove(&7));
    }

    #[test]
    fn a_tab_past_the_end_means_the_last_one() {
        let mut tabs = list(&[1, 2, 3], 0);
        tabs.activate(8);
        assert_eq!(*tabs.shown(), 3);
    }

    #[test]
    fn stepping_wraps_both_ways() {
        let tabs = list(&[1, 2, 3], 2);
        assert_eq!(tabs.step(1), 0);
        assert_eq!(tabs.step(-1), 1);
        assert_eq!(list(&[1, 2, 3], 0).step(-1), 2);
    }
}
