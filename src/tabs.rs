//! The tabs of one pane: several locations in one tile of the split tree,
//! one of them shown. Zed's model, in which each pane holds a list of items.
//!
//! This holds only the list and which item is shown. The workspace makes
//! every change, because a change of tab is also a change of focus, of the
//! active pane and of which panes watch their folders.

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, DragMoveEvent, Pixels, Point, Context, Entity, EventEmitter, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px, relative,
};
use theme::ActiveTheme;

use std::path::Path;

use crate::dir_pane::{DirPane, folder_drop_target};
use crate::icon::Icon;
use crate::location::Location;
use crate::pane_group::SplitDirection;
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

    /// Put `item` at `at`, or at the end when `at` is past it. The shown tab
    /// does not change.
    pub fn insert(&mut self, at: usize, item: T) -> usize {
        let at = at.min(self.items.len());
        self.items.insert(at, item);
        if at <= self.active {
            self.active += 1;
        }
        at
    }

    /// Move the tab at `from` so it lands where the tab at `to` was, as a tab
    /// dropped on another tab does in Zed. The shown tab stays shown.
    pub fn move_to(&mut self, from: usize, to: usize) {
        if from >= self.items.len() {
            return;
        }
        let to = to.min(self.items.len() - 1);
        let shown = self.items[self.active].clone();
        let item = self.items.remove(from);
        self.items.insert(to, item);
        self.active = self.index_of(&shown).unwrap_or(0);
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
    /// Set by the workspace while the window has more than one pane: then
    /// every pane shows its bar, so even a lone tab can be dragged.
    split_bar: bool,
    /// Where a tab dragged over the pane below would split off, or `None`
    /// to join this pane. Read from the pointer, as Zed's
    /// `drag_split_direction`.
    drag_split: Option<SplitDirection>,
    /// Where the pane below the bar was drawn, as the last drag over it saw.
    body: Option<Bounds<Pixels>>,
}

/// A tab on its way somewhere, as Zed's `DraggedTab`: what is moving, and
/// what the preview under the pointer shows. The tile it leaves is looked up
/// at the drop.
#[derive(Clone)]
pub struct DraggedTab {
    pub pane: Entity<DirPane>,
    pub ix: usize,
    title: String,
    archive: bool,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = tab_label(&self.title, self.archive, true, cx);
        ui::tab("dragged-tab", TabPosition::First, true, label, None, cx)
    }
}

/// What a press on the bar asks for. The workspace does it, see the module
/// comment.
pub enum TabsEvent {
    Activate(usize),
    Close(usize),
    New,
    /// A tab dropped on this tile: on a tab or the end of the bar to go at
    /// `Some(ix)`, or on the pane below (`None`) to go after the shown tab.
    ///
    /// `split` makes a new pane on that side of this one for the tab instead.
    /// `copy` (ctrl held) leaves the tab where it was and drops a new one on
    /// the same folder, as Zed clones an item.
    Drop {
        dragged: DraggedTab,
        at: Option<usize>,
        split: Option<SplitDirection>,
        copy: bool,
    },
}

impl EventEmitter<TabsEvent> for Tabs {}

impl Tabs {
    pub fn new(first: Entity<DirPane>) -> Self {
        Self {
            list: TabList::new(first),
            scroll: ScrollHandle::new(),
            split_bar: false,
            drag_split: None,
            body: None,
        }
    }

    /// The side a dragged tab would split off on, for the probe.
    pub fn drag_split(&self) -> Option<SplitDirection> {
        self.drag_split
    }

    /// Follow a dragged tab over the pane, as Zed's `handle_drag_move`.
    fn track_drag(&mut self, event: &DragMoveEvent<DraggedTab>, cx: &mut Context<Self>) {
        self.body = Some(event.bounds);
        let split = self.split_at(event.event.position, &event.drag(cx).pane);
        if split != self.drag_split {
            self.drag_split = split;
            cx.notify();
        }
    }

    /// The side a tab dragged to `at` would split off on, from where the pane
    /// was last drawn.
    fn split_at(&self, at: Point<Pixels>, pane: &Entity<DirPane>) -> Option<SplitDirection> {
        let bounds = self.body?;
        // A tab alone in its pane has nowhere to split off to: the pane it
        // would leave is the one it would split.
        let alone = self.list.items().len() == 1 && self.list.shown() == pane;
        if alone || !bounds.contains(&at) {
            return None;
        }
        split_side(
            f32::from(at.x - bounds.left()),
            f32::from(at.y - bounds.top()),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        )
    }

    /// What a drop asks for, read when it lands.
    fn emit_drop(
        &mut self,
        dragged: &DraggedTab,
        at: Option<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        // From where the pointer is now, not from the last move: the drop is
        // what counts, and the highlight could trail it.
        let split = match at {
            None => self.split_at(window.mouse_position(), &dragged.pane),
            Some(_) => None,
        };
        #[cfg(debug_assertions)]
        eprintln!(
            "[hoja] tab drop at {:?} on {:?}: shown {:?}, dropped {split:?}",
            window.mouse_position(),
            self.body,
            self.drag_split,
        );
        self.drag_split = None;
        cx.emit(TabsEvent::Drop {
            dragged: dragged.clone(),
            at,
            split,
            copy: window.modifiers().control,
        });
        cx.notify();
    }

    /// Whether the bar is drawn: with a second tab, or in a split.
    pub fn has_bar(&self) -> bool {
        self.list.items().len() > 1 || self.split_bar
    }

    pub fn set_split_bar(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.split_bar != on {
            self.split_bar = on;
            cx.notify();
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
        if !self.has_bar() {
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
                let archive = matches!(location, Location::Archive { .. });
                let label = tab_label(&title(location), archive, selected && focused, cx);
                let dragged = DraggedTab {
                    pane: pane.clone(),
                    ix,
                    title: title(location),
                    archive,
                };
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
                // Something dropped on a tab goes into its folder, as onto a
                // folder row. A tab in an archive takes nothing.
                .when_some(location.disk().map(Path::to_path_buf), |el, target| {
                    folder_drop_target(el, pane.clone(), target, cx)
                })
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(TabsEvent::Activate(ix));
                }))
                .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                // Zed's mark: the tab tinted, with a rule on the side the
                // dragged tab would land.
                .drag_over::<DraggedTab>(move |el, dragged, _, cx| {
                    let colors = cx.theme().colors();
                    let el = el
                        .bg(colors.drop_target_background)
                        .border_color(colors.drop_target_border)
                        .border_0();
                    if ix < dragged.ix {
                        el.border_l_2()
                    } else if ix > dragged.ix {
                        el.border_r_2()
                    } else {
                        el
                    }
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.emit_drop(dragged, Some(ix), window, cx);
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
                .drag_over::<DraggedTab>(|el, _, _, cx| {
                    el.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.emit_drop(dragged, Some(count), window, cx);
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

/// A tab's icon and name. Only an archive gets an icon, as Zed shows none: a
/// folder inside one is named like any folder on disk. `lit` is the shown tab
/// of the focused pane; every other label is muted.
fn tab_label(title: &str, archive: bool, lit: bool, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .when(archive, |el| {
            el.child(
                Icon::from_path("icons/file_icons/archive.svg", colors.text_muted).size(px(14.)),
            )
        })
        .child(
            div()
                .when(!lit, |el| el.text_color(colors.text_muted))
                .child(title.to_string()),
        )
        .into_any_element()
}

/// What a tab is called: the last name in its location, as the window title.
pub fn title(location: &Location) -> String {
    let key = location.key();
    key.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| key.display().to_string())
}

/// How far in from a pane's edge a dragged tab splits rather than joins, as a
/// share of the pane's shorter side: Zed's `drop_target_size` default.
const SPLIT_ZONE: f32 = 0.2;

/// Zed's rule for where a tab dragged to `(x, y)` in a pane of `width` by
/// `height` would split off: near an edge, the nearest edge; further in,
/// nowhere, and the tab joins the pane.
fn split_side(x: f32, y: f32, width: f32, height: f32) -> Option<SplitDirection> {
    let zone = width.min(height) * SPLIT_ZONE;
    if x >= zone && x <= width - zone && y >= zone && y <= height - zone {
        return None;
    }
    [
        (SplitDirection::Up, y),
        (SplitDirection::Right, width - x),
        (SplitDirection::Down, height - y),
        (SplitDirection::Left, x),
    ]
    .into_iter()
    .min_by(|a, b| a.1.total_cmp(&b.1))
    .map(|(side, _)| side)
}

impl Render for Tabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The bar only once there is a second tab: one tab is a pane as it
        // was before there were tabs.
        let bar = self.has_bar().then(|| self.render_bar(cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(bar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .on_drag_move::<DraggedTab>(cx.listener(|this, event, _, cx| {
                        this.track_drag(event, cx);
                    }))
                    // Cached as the tree caches a leaf, so a tab re-renders
                    // only when it notifies, not each time the bar above it
                    // does.
                    .child(self.shown().clone().cached(crate::pane_group::fill_style()))
                    // Zed's drop target over the pane, clear until a tab is
                    // dragged over it. Zed hides it until its pane is under a
                    // drag, but a hidden element takes no drop, and the
                    // scrollbar along a pane's right edge blocks the pane
                    // from being under anything: a drop at that edge, the one
                    // a split to the right is made at, was lost. Drawn last,
                    // it is above the scrollbar; its hitbox blocks nothing,
                    // so the rows under it still take clicks and file drops.
                    .child(
                        div()
                            .id("tab-drop-target")
                            .absolute()
                            // The whole pane to join it, or the half the new
                            // pane would take, as in Zed.
                            .map(|el| match self.drag_split {
                                None => el.top_0().left_0().size_full(),
                                Some(SplitDirection::Up) => {
                                    el.top_0().left_0().w_full().h(relative(0.5))
                                }
                                Some(SplitDirection::Down) => {
                                    el.bottom_0().left_0().w_full().h(relative(0.5))
                                }
                                Some(SplitDirection::Left) => {
                                    el.top_0().left_0().h_full().w(relative(0.5))
                                }
                                Some(SplitDirection::Right) => {
                                    el.top_0().right_0().h_full().w(relative(0.5))
                                }
                            })
                            .drag_over::<DraggedTab>(|el, _, _, cx| {
                                el.bg(cx.theme().colors().drop_target_background)
                            })
                            .on_drop(cx.listener(|this, dragged: &DraggedTab, window, cx| {
                                this.emit_drop(dragged, None, window, cx);
                            })),
                    ),
            )
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
    fn inserting_before_the_shown_tab_keeps_it_shown() {
        let mut tabs = list(&[1, 2, 3], 1);
        assert_eq!(tabs.insert(0, 9), 0);
        assert_eq!(tabs.items(), &[9, 1, 2, 3]);
        assert_eq!(*tabs.shown(), 2);
        assert_eq!(tabs.insert(99, 8), 4);
        assert_eq!(*tabs.shown(), 2);
    }

    #[test]
    fn a_moved_tab_lands_where_the_other_was_and_the_shown_one_stays() {
        let mut tabs = list(&[1, 2, 3], 0);
        tabs.move_to(0, 2);
        assert_eq!(tabs.items(), &[2, 3, 1]);
        assert_eq!(*tabs.shown(), 1);

        let mut tabs = list(&[1, 2, 3], 1);
        tabs.move_to(2, 0);
        assert_eq!(tabs.items(), &[3, 1, 2]);
        assert_eq!(*tabs.shown(), 2);
        // The end of the bar is past the last tab.
        tabs.move_to(0, 3);
        assert_eq!(tabs.items(), &[1, 2, 3]);
    }

    #[test]
    fn a_tab_near_an_edge_splits_toward_it() {
        // 400 by 200: the zone is 40 pixels in from each edge.
        assert_eq!(split_side(200., 5., 400., 200.), Some(SplitDirection::Up));
        assert_eq!(split_side(390., 100., 400., 200.), Some(SplitDirection::Right));
        assert_eq!(split_side(200., 170., 400., 200.), Some(SplitDirection::Down));
        assert_eq!(split_side(30., 100., 400., 200.), Some(SplitDirection::Left));
        assert_eq!(split_side(200., 100., 400., 200.), None);
        assert_eq!(split_side(41., 41., 400., 200.), None);
        // In a corner, the nearer edge.
        assert_eq!(split_side(10., 5., 400., 200.), Some(SplitDirection::Up));
        assert_eq!(split_side(5., 10., 400., 200.), Some(SplitDirection::Left));
    }

    #[test]
    fn stepping_wraps_both_ways() {
        let tabs = list(&[1, 2, 3], 2);
        assert_eq!(tabs.step(1), 0);
        assert_eq!(tabs.step(-1), 1);
        assert_eq!(list(&[1, 2, 3], 0).step(-1), 2);
    }
}
