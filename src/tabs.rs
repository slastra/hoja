//! The tabs of one pane: several locations in one tile of the split tree,
//! one of them shown. Zed's model, in which each pane holds a list of items.
//!
//! This holds only the list and which item is shown. The workspace makes
//! every change, because a change of tab is also a change of focus, of the
//! active pane and of which panes watch their folders.

use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px,
};
use theme::ActiveTheme;

use crate::dir_pane::DirPane;
use crate::icon::Icon;
use crate::location::Location;
use crate::ui::{self, TabPosition};

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
    /// The bar's sideways scroll, which also knows where each tab was drawn.
    scroll: ScrollHandle,
}

/// What a press on the bar asks for. The workspace does it, see the module
/// comment.
pub enum TabsEvent {
    Activate(usize),
    Close(usize),
    New,
}

impl EventEmitter<TabsEvent> for Tabs {}

impl Tabs {
    pub fn new(first: Entity<DirPane>) -> Self {
        Self {
            list: TabList::new(first),
            scroll: ScrollHandle::new(),
        }
    }

    pub fn shown(&self) -> &Entity<DirPane> {
        self.list.shown()
    }

    /// Bring the shown tab into view along the bar, after a key changed it.
    pub fn reveal_shown(&self) {
        self.scroll.scroll_to_item(self.list.active());
    }

    /// Where each tab was last drawn, as `[x, y, width, height]` in window
    /// pixels, for the probe. Empty while there is no bar.
    pub fn rects(&self) -> Vec<[i32; 4]> {
        if self.list.items().len() < 2 {
            return Vec::new();
        }
        (0..self.list.items().len())
            .filter_map(|ix| self.scroll.bounds_for_item(ix))
            .map(|b| {
                [
                    f32::from(b.origin.x) as i32,
                    f32::from(b.origin.y) as i32,
                    f32::from(b.size.width) as i32,
                    f32::from(b.size.height) as i32,
                ]
            })
            .collect()
    }

    fn render_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let count = self.list.items().len();
        let active = self.list.active();
        // A pane that is not the focused one dims its tabs, as it dims its
        // path in the toolbar.
        let focused = self.shown().read(cx).is_active();
        let mut tabs: Vec<AnyElement> = self
            .list
            .items()
            .iter()
            .enumerate()
            .map(|(ix, pane)| {
                let location = pane.read(cx).location();
                let selected = ix == active;
                let lit = selected && focused;
                let label = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(Icon::from_path(icon_for(location), colors.text_muted).size(px(14.)))
                    .child(
                        div()
                            .when(!lit, |el| el.text_color(colors.text_muted))
                            .child(title(location)),
                    )
                    .into_any_element();
                let close = ui::icon_button(("close-tab", ix), "icons/file_icons/close.svg", cx)
                    .invisible()
                    .group_hover(ui::TAB_GROUP, |s| s.visible())
                    .tooltip(ui::tooltip("Close Tab"))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(TabsEvent::Close(ix));
                    }))
                    .into_any_element();
                ui::tab(
                    ("tab", ix),
                    TabPosition::of(ix, count, active),
                    selected,
                    label,
                    Some(close),
                    cx,
                )
                .tooltip(ui::tooltip(location.to_string()))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(TabsEvent::Activate(ix));
                }))
                // Middle click closes a tab, as in Zed and in browsers.
                .on_aux_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                    if event.is_middle_click() {
                        cx.stop_propagation();
                        cx.emit(TabsEvent::Close(ix));
                    }
                }))
                .into_any_element()
            })
            .collect();
        // The rest of the row, where a double click opens a tab, as in Zed.
        tabs.push(
            div()
                .id("tab-bar-end")
                .flex_1()
                .min_w_6()
                .h_full()
                .on_click(cx.listener(|_, event: &ClickEvent, _, cx| {
                    if event.click_count() == 2 {
                        cx.emit(TabsEvent::New);
                    }
                }))
                .into_any_element(),
        );
        // The bar is outside the pane, so a press on it would give the keys
        // to the nearest thing that takes focus, which is the window, and
        // ctrl-t would then do nothing. In Zed the pane's focus covers its
        // tab bar; the same here, and pressing another pane's bar moves to
        // that pane, as in Zed.
        let shown = self.shown().focus_handle(cx);
        ui::tab_bar("tab-bar", tabs, &self.scroll, cx).on_any_mouse_down(move |_, window, cx| {
            window.prevent_default();
            window.focus(&shown, cx);
        })
    }
}

/// What a tab is called: the last name in its location, as the window title.
pub fn title(location: &Location) -> String {
    let key = location.key();
    key.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| key.display().to_string())
}

fn icon_for(location: &Location) -> &'static str {
    match location {
        Location::Disk(_) => "icons/file_icons/folder.svg",
        Location::Archive { .. } => "icons/file_icons/archive.svg",
    }
}

impl Render for Tabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The bar only once there is a second tab: one tab is a pane as it
        // was before there were tabs.
        let bar = (self.list.items().len() > 1).then(|| self.render_bar(cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(bar)
            // Cached as the tree caches a leaf, so a tab re-renders only when
            // it notifies, not each time the bar above it does.
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
