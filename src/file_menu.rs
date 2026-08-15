//! Minimal context menu built from gpui primitives: no `ui` crate.
//!
//! Mechanics follow Zed's `RightClickMenu`/`ContextMenu` skeleton: the owner
//! stores `(position, Entity<FileMenu>)`, renders it through
//! `deferred(anchored().position(p)...)`, and the menu dismisses through three
//! independent paths (blur, click-outside, Escape). Two load-bearing quirks
//! inherited from Zed:
//!
//! - Focusing a deferred element requires a **double-nested**
//!   `window.on_next_frame`, the deferred subtree isn't in the dispatch tree
//!   until after the deferred callback runs.
//! - An item handler must **refocus the originating pane** before dispatching
//!   its action, or the action dispatches at the menu's focus node where no
//!   handler exists.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Window, actions, anchored, deferred, div, prelude::*, px, relative,
};
use theme::ActiveTheme;

// The `menu` key context is bound in main.rs: escape/enter/up/down.
actions!(
    menu,
    [
        Cancel,
        Confirm,
        SelectNext,
        SelectPrevious,
        OpenSubmenu,
        CloseSubmenu
    ]
);

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

pub enum MenuItem {
    Action {
        label: SharedString,
        handler: Handler,
        disabled: bool,
        /// `Some(state)` renders a check slot: toggles and radio groups.
        checked: Option<bool>,
    },
    Separator,
    /// A row that opens a menu of its own.
    ///
    /// **One level only.** `FileMenu` tracks the open submenu as a single
    /// index, so a `Submenu` nested inside another would navigate wrongly
    /// rather than fail loudly. Nothing in the tree builds one, and the
    /// renderer draws a nested one as a plain row that does nothing.
    Submenu {
        label: SharedString,
        items: Vec<MenuItem>,
    },
}

/// A row's text, or `None` for a separator, which has none.
fn label_of(item: &MenuItem) -> Option<String> {
    match item {
        MenuItem::Action { label, .. } | MenuItem::Submenu { label, .. } => Some(label.to_string()),
        MenuItem::Separator => None,
    }
}

/// The next selectable index in `items`, wrapping, or `None` if nothing in the
/// list can be selected at all. Shared by the menu and its submenu so the
/// skip-separators-and-disabled rule has one definition.
fn step(items: &[MenuItem], from: Option<usize>, delta: isize) -> Option<usize> {
    let len = items.len() as isize;
    if len == 0 {
        return None;
    }
    let mut ix = from.map(|i| i as isize).unwrap_or(-delta);
    for _ in 0..len {
        ix = (ix + delta).rem_euclid(len);
        if items[ix as usize].selectable() {
            return Some(ix as usize);
        }
    }
    None
}

impl MenuItem {
    pub fn action(
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self::Action {
            label: label.into(),
            handler: Rc::new(handler),
            disabled: false,
            checked: None,
        }
    }

    /// A checkable item: toggles and radio-group members.
    pub fn toggle(
        label: impl Into<SharedString>,
        checked: bool,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self::Action {
            label: label.into(),
            handler: Rc::new(handler),
            disabled: false,
            checked: Some(checked),
        }
    }

    #[allow(dead_code)] // menu vocabulary; useful the moment an item needs gating
    pub fn disabled(label: impl Into<SharedString>) -> Self {
        Self::Action {
            label: label.into(),
            handler: Rc::new(|_, _| {}),
            disabled: true,
            checked: None,
        }
    }

    /// A row that opens a menu of its own. One level only: see the variant.
    pub fn submenu(label: impl Into<SharedString>, items: Vec<MenuItem>) -> Self {
        Self::Submenu {
            label: label.into(),
            items,
        }
    }

    fn selectable(&self) -> bool {
        match self {
            Self::Action { disabled, .. } => !disabled,
            // Selectable but not activating: Enter and Right open it.
            Self::Submenu { .. } => true,
            Self::Separator => false,
        }
    }
}

/// The part of a menu that is only bookkeeping: which row is selected, which
/// submenu is open, and where the keyboard sits inside it.
///
/// Split out from `FileMenu` because none of it needs a window, a focus handle
/// or a context — which is the only reason the navigation rules are testable at
/// all, since the menu itself cannot be built outside a running app.
#[derive(Default)]
struct Nav {
    items: Vec<MenuItem>,
    selected: Option<usize>,
    /// Index into `items` of the open `Submenu`, if one is open.
    open: Option<usize>,
    /// Keyboard position inside the open submenu.
    sub_selected: Option<usize>,
    /// Whether the pointer is over the submenu. See `dismiss_on_out_press`.
    hovered: bool,
}

impl Nav {
    fn new(items: Vec<MenuItem>) -> Self {
        Self {
            items,
            ..Default::default()
        }
    }

    /// The items of the open submenu, if one is open.
    fn open_items(&self) -> Option<&[MenuItem]> {
        match self.items.get(self.open?) {
            Some(MenuItem::Submenu { items, .. }) => Some(items),
            _ => None,
        }
    }

    /// Open the submenu on the selected row. True if one opened.
    fn open_selected(&mut self) -> bool {
        let Some(ix) = self.selected else {
            return false;
        };
        let Some(MenuItem::Submenu { items, .. }) = self.items.get(ix) else {
            return false;
        };
        // Land on the first row that can be chosen, never a leading separator.
        self.sub_selected = step(items, None, 1);
        self.open = Some(ix);
        true
    }

    /// Shut the open submenu. True if there was one.
    fn close(&mut self) -> bool {
        self.sub_selected = None;
        self.hovered = false;
        self.open.take().is_some()
    }

    fn move_selection(&mut self, delta: isize) {
        // While a submenu is open the arrows move inside it and leave the
        // parent selection alone, so closing lands back on the row that opened
        // it rather than somewhere the user never navigated to.
        if self.open.is_some() {
            let next = self
                .open_items()
                .and_then(|items| step(items, self.sub_selected, delta));
            if next.is_some() {
                self.sub_selected = next;
            }
            return;
        }
        if let Some(next) = step(&self.items, self.selected, delta) {
            self.selected = Some(next);
        }
    }

    /// Whether a press landing outside the menu should dismiss it.
    ///
    /// The submenu is drawn outside the menu's own bounds, so a click on one of
    /// its rows arrives as an out-press. Dismissing on that would tear the menu
    /// down before the row's handler ran, and the application would never launch.
    fn dismiss_on_out_press(&self) -> bool {
        !self.hovered
    }

    /// The handler Enter should run, if any.
    fn confirmed(&self) -> Option<Handler> {
        let (items, ix) = match self.open {
            Some(_) => (self.open_items()?, self.sub_selected?),
            None => (self.items.as_slice(), self.selected?),
        };
        match items.get(ix) {
            Some(MenuItem::Action {
                handler,
                disabled: false,
                ..
            }) => Some(handler.clone()),
            _ => None,
        }
    }

    fn insert(&mut self, ix: usize, items: Vec<MenuItem>) {
        // Keyboard selection is an index into the same vector: shift it so the
        // highlight stays on the row the user picked.
        if let Some(selected) = self.selected.filter(|&s| s >= ix) {
            self.selected = Some(selected + items.len());
        }
        if let Some(open) = self.open.filter(|&o| o >= ix) {
            self.open = Some(open + items.len());
        }
        self.items.splice(ix..ix, items);
    }

    fn labels(&self) -> Vec<String> {
        self.items.iter().filter_map(label_of).collect()
    }

    fn submenu_labels(&self) -> Option<Vec<String>> {
        Some(self.open_items()?.iter().filter_map(label_of).collect())
    }
}

pub struct FileMenu {
    nav: Nav,
    focus_handle: FocusHandle,
    _blur_subscription: Subscription,
}

impl FileMenu {
    pub fn new(items: Vec<MenuItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        // Focus loss dismisses, this covers alt-tab, pane clicks that somehow
        // miss `on_mouse_down_out`, and keyboard focus moves.
        let _blur_subscription =
            cx.on_blur(&focus_handle, window, |_, _, cx| cx.emit(DismissEvent));
        window.refresh();

        Self {
            nav: Nav::new(items),
            focus_handle,
            _blur_subscription,
        }
    }

    /// Splice items in after the menu is already on screen. Used by the
    /// "Open With" section, which is resolved off the UI thread.
    pub fn insert_items(&mut self, ix: usize, items: Vec<MenuItem>, cx: &mut Context<Self>) {
        if items.is_empty() || ix > self.nav.items.len() {
            return;
        }
        self.nav.insert(ix, items);
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        // Escape backs out of the submenu first. One press should not throw the
        // whole menu away when the user only meant to leave one row.
        if self.nav.close() {
            cx.notify();
            return;
        }
        cx.emit(DismissEvent);
    }

    /// Shut the open submenu. True if there was one.
    /// Row labels top to bottom, separators omitted. For the probe: a test
    /// needs to assert what is on offer, not merely that something is.
    pub fn item_labels(&self) -> Vec<String> {
        self.nav.labels()
    }

    /// The open submenu's labels, or `None` when nothing is open.
    pub fn open_submenu_labels(&self) -> Option<Vec<String>> {
        self.nav.submenu_labels()
    }

    fn open_submenu_action(
        &mut self,
        _: &OpenSubmenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.nav.open_selected() {
            cx.notify();
        }
    }

    fn dismiss_submenu(&mut self, _: &CloseSubmenu, _window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.close() {
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        // Enter on a closed submenu row opens it rather than doing nothing.
        if self.nav.open.is_none()
            && matches!(
                self.nav.selected.and_then(|ix| self.nav.items.get(ix)),
                Some(MenuItem::Submenu { .. })
            )
        {
            self.open_submenu_action(&OpenSubmenu, window, cx);
            return;
        }
        if let Some(handler) = self.nav.confirmed() {
            handler(window, cx);
        } else if self.nav.open.is_some() {
            // Inside a submenu with nothing chosen, hold rather than throwing
            // the menu away on a keypress that meant nothing.
            return;
        }
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.nav.move_selection(1);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.nav.move_selection(-1);
        cx.notify();
    }

    /// The panel that hangs off an open `Submenu` row.
    ///
    /// Rows here are plain actions. Nesting is unsupported (see the variant), so
    /// a `Submenu` found inside one is drawn inert rather than silently dropped.
    fn render_submenu(&self, items: &[MenuItem], cx: &Context<Self>) -> AnyElement {
        // Copied out rather than held: `colors` would borrow `cx`, which the
        // listeners below need mutably.
        let (border, surface, text, text_muted, chosen, hovered) = {
            let c = cx.theme().colors();
            (
                c.border,
                c.elevated_surface_background,
                c.text,
                c.text_muted,
                c.element_selected,
                c.element_hover,
            )
        };
        // Over the submenu's own items, not the parent menu's: a submenu of
        // toggles reserves the slot and a submenu of plain rows does not, the
        // same rule the menu applies to itself.
        let reserve_check_slot = items.iter().any(|item| {
            matches!(
                item,
                MenuItem::Action {
                    checked: Some(_),
                    ..
                }
            )
        });
        div()
            // `overflow_y_scroll` below lives on StatefulInteractiveElement, so
            // this needs an id before it can scroll at all.
            .id("submenu-panel")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(180.))
            // A type with thirty handlers exists; scrolling beats running off
            // the bottom of the screen with no way to reach the rest.
            .max_h(px(420.))
            .overflow_y_scroll()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(surface)
            .shadow_lg()
            .on_hover(cx.listener(|this, over: &bool, _, cx| {
                this.nav.hovered = *over;
                cx.notify();
            }))
            .children(items.iter().enumerate().map(|(sub_ix, item)| {
                match item {
                    MenuItem::Separator => div().my_1().h(px(1.)).bg(border).into_any_element(),
                    MenuItem::Action {
                        label,
                        handler,
                        disabled,
                        checked,
                    } => {
                        let handler = handler.clone();
                        let disabled = *disabled;
                        let checked = *checked;
                        div()
                            .id(("submenu", sub_ix))
                            .px_3()
                            .py_0p5()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1p5()
                            .when(self.nav.sub_selected == Some(sub_ix), |el| el.bg(chosen))
                            .when(disabled, |el| el.text_color(text_muted))
                            .when(!disabled, |el| {
                                el.cursor_pointer()
                                    .hover(|s| s.bg(hovered))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        handler(window, cx);
                                        cx.emit(DismissEvent);
                                    }))
                            })
                            .when(reserve_check_slot, |el| {
                                el.child(div().w(px(14.)).flex_none().when(
                                    checked == Some(true),
                                    |slot| {
                                        slot.child(crate::icon::Icon::from_path(
                                            "icons/file_icons/check.svg",
                                            text,
                                        ))
                                    },
                                ))
                            })
                            .child(label.clone())
                            .into_any_element()
                    }
                    MenuItem::Submenu { label, .. } => div()
                        .px_3()
                        .py_0p5()
                        .text_color(text_muted)
                        .child(label.clone())
                        .into_any_element(),
                }
            }))
            .into_any_element()
    }
}

impl Focusable for FileMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FileMenu {}

impl Render for FileMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        // Slot reservation is a property of the menu, not the item: without it
        // a plain item in a menu of toggles sits left of everything else.
        let reserve_check_slot = self.nav.items.iter().any(|item| {
            matches!(
                item,
                MenuItem::Action {
                    checked: Some(_),
                    ..
                }
            )
        });

        div()
            // Clicks on the menu must not fall through to rows underneath.
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("menu")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_submenu_action))
            .on_action(cx.listener(Self::dismiss_submenu))
            // Fires in the capture phase when the press lands outside our bounds.
            // The submenu is drawn outside them, so a click on one of its rows
            // arrives here first and would dismiss before the row's own handler
            // ran; `submenu_hovered` is how we tell that press apart.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if !this.nav.dismiss_on_out_press() {
                    return;
                }
                cx.emit(DismissEvent);
            }))
            .flex()
            .flex_col()
            .min_w(px(180.))
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .text_sm()
            .text_color(colors.text)
            .children(
                self.nav
                    .items
                    .iter()
                    .enumerate()
                    .map(|(ix, item)| match item {
                        MenuItem::Separator => {
                            div().my_1().h(px(1.)).bg(colors.border).into_any_element()
                        }
                        MenuItem::Action {
                            label,
                            handler,
                            disabled,
                            checked,
                        } => {
                            let handler = handler.clone();
                            let disabled = *disabled;
                            let checked = *checked;
                            let selected = self.nav.selected == Some(ix);
                            div()
                                .id(ix)
                                .px_3()
                                .py_0p5()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .when(selected, |el| el.bg(colors.element_selected))
                                .when(disabled, |el| el.text_color(colors.text_muted))
                                .when(!disabled, |el| {
                                    el.cursor_pointer()
                                        .hover(|s| s.bg(colors.element_hover))
                                        .on_hover(cx.listener(|this, over: &bool, _, cx| {
                                            if *over && this.nav.close() {
                                                cx.notify();
                                            }
                                        }))
                                        .on_click(cx.listener(move |_, _, window, cx| {
                                            handler(window, cx);
                                            cx.emit(DismissEvent);
                                        }))
                                })
                                .when(reserve_check_slot, |el| {
                                    el.child(div().w(px(14.)).flex_none().when(
                                        checked == Some(true),
                                        |slot| {
                                            slot.child(crate::icon::Icon::from_path(
                                                "icons/file_icons/check.svg",
                                                colors.text,
                                            ))
                                        },
                                    ))
                                })
                                .child(label.clone())
                                .into_any_element()
                        }
                        MenuItem::Submenu { label, items } => {
                            let selected = self.nav.selected == Some(ix);
                            let open = self.nav.open == Some(ix);
                            div()
                                .id(ix)
                                .relative()
                                .px_3()
                                .py_0p5()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1p5()
                                .cursor_pointer()
                                // An open row stays lit: the row and its panel are one
                                // thing, and unlighting it makes the panel look orphaned.
                                .when(selected || open, |el| el.bg(colors.element_selected))
                                .hover(|s| s.bg(colors.element_hover))
                                .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                                    if *over && this.nav.open != Some(ix) {
                                        this.nav.selected = Some(ix);
                                        this.nav.sub_selected = None;
                                        this.nav.open = Some(ix);
                                        cx.notify();
                                    }
                                }))
                                .when(reserve_check_slot, |el| {
                                    el.child(div().w(px(14.)).flex_none())
                                })
                                .child(label.clone())
                                // Pushes the chevron hard right, so the row reads as
                                // "there is more this way".
                                .child(div().flex_1().min_w(px(12.)))
                                .child(crate::icon::Icon::from_path(
                                    "icons/file_icons/chevron_right.svg",
                                    colors.text_muted,
                                ))
                                .when(open, |el| {
                                    // `anchored()` with no explicit position anchors
                                    // where it lands, so this opens at the row's right
                                    // edge without measuring anything. Snapping rather
                                    // than the default corner-switch, because a long
                                    // list opened low on the screen would otherwise run
                                    // off the bottom with no way to reach the rest;
                                    // snapping slides it back on both axes.
                                    el.child(
                                        div().absolute().left(relative(1.)).top(px(-4.)).child(
                                            deferred(
                                                anchored()
                                                    .snap_to_window_with_margin(px(8.))
                                                    .child(self.render_submenu(items, cx)),
                                            )
                                            .with_priority(2),
                                        ),
                                    )
                                })
                                .into_any_element()
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(label: &str) -> MenuItem {
        MenuItem::action(label, |_, _| {})
    }

    /// "Open" then an "Open With" submenu whose first row is a separator.
    fn menu() -> Nav {
        Nav::new(vec![
            action("Open"),
            MenuItem::submenu(
                "Open With",
                vec![MenuItem::Separator, action("feh"), action("GIMP")],
            ),
        ])
    }

    #[test]
    fn navigation_skips_separators_and_disabled_rows() {
        let items = vec![
            action("a"),
            MenuItem::Separator,
            MenuItem::disabled("b"),
            action("c"),
        ];
        assert_eq!(step(&items, None, 1), Some(0));
        assert_eq!(step(&items, Some(0), 1), Some(3), "past both");
        assert_eq!(step(&items, Some(3), 1), Some(0), "wraps");
        assert_eq!(step(&items, Some(0), -1), Some(3), "and backwards");
        assert_eq!(
            step(&[MenuItem::Separator], None, 1),
            None,
            "nothing to pick"
        );
    }

    #[test]
    fn opening_a_submenu_lands_on_the_first_choosable_row() {
        let mut nav = menu();
        nav.selected = Some(1);
        assert!(nav.open_selected());
        assert_eq!(nav.open, Some(1));
        // Index 1, not 0: index 0 is a separator, and landing there would
        // highlight nothing and make the first Enter do nothing.
        assert_eq!(nav.sub_selected, Some(1));
    }

    #[test]
    fn only_a_submenu_row_opens_anything() {
        let mut nav = menu();
        nav.selected = Some(0);
        assert!(!nav.open_selected(), "\"Open\" is not a submenu");
        assert_eq!(nav.open, None);
    }

    #[test]
    fn closing_leaves_the_selection_on_the_row_that_opened_it() {
        let mut nav = menu();
        nav.selected = Some(1);
        nav.open_selected();
        nav.move_selection(1);
        assert!(nav.close());
        assert_eq!(nav.selected, Some(1), "back on the Open With row");
        assert_eq!(nav.sub_selected, None);
        assert!(!nav.close(), "and there is nothing left to close");
    }

    #[test]
    fn arrows_move_inside_an_open_submenu_and_leave_the_parent_alone() {
        let mut nav = menu();
        nav.selected = Some(1);
        nav.open_selected();
        assert_eq!(nav.sub_selected, Some(1));
        nav.move_selection(1);
        assert_eq!(nav.sub_selected, Some(2));
        nav.move_selection(1);
        assert_eq!(nav.sub_selected, Some(1), "wraps past the separator");
        assert_eq!(nav.selected, Some(1), "the parent selection never moved");
    }

    #[test]
    fn enter_chooses_from_the_submenu_once_it_is_open() {
        let mut nav = menu();
        nav.selected = Some(0);
        assert!(nav.confirmed().is_some(), "a plain row activates");
        nav.selected = Some(1);
        assert!(
            nav.confirmed().is_none(),
            "a submenu row activates nothing itself; Enter opens it"
        );
        nav.open_selected();
        assert!(nav.confirmed().is_some(), "and now it chooses from inside");
    }

    #[test]
    fn a_press_over_the_submenu_is_not_a_press_outside_the_menu() {
        // The submenu is drawn outside the menu's own bounds, so clicking one
        // of its rows arrives as an out-press. Dismissing on that would tear
        // the menu down before the row's handler ran and nothing would launch.
        let mut nav = menu();
        assert!(nav.dismiss_on_out_press());
        nav.hovered = true;
        assert!(!nav.dismiss_on_out_press());
    }

    #[test]
    fn splicing_rows_in_shifts_an_open_submenu_with_them() {
        // "Open With" is spliced in after the menu is already on screen, so the
        // indices it holds have to survive a later insertion above them.
        let mut nav = menu();
        nav.selected = Some(1);
        nav.open_selected();
        nav.insert(0, vec![action("New Folder")]);
        assert_eq!(
            nav.open,
            Some(2),
            "the open submenu moved down with its row"
        );
        assert_eq!(nav.selected, Some(2));
    }
}
