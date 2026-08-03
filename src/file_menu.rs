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
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    Window, actions, div, prelude::*, px,
};
use theme::ActiveTheme;

// The `menu` key context is bound in main.rs: escape/enter/up/down.
actions!(menu, [Cancel, Confirm, SelectNext, SelectPrevious]);

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

    fn selectable(&self) -> bool {
        matches!(
            self,
            Self::Action {
                disabled: false,
                ..
            }
        )
    }
}

pub struct FileMenu {
    items: Vec<MenuItem>,
    focus_handle: FocusHandle,
    selected: Option<usize>,
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
            items,
            focus_handle,
            selected: None,
            _blur_subscription,
        }
    }

    /// Splice items in after the menu is already on screen. Used by the
    /// "Open With" section, which is resolved off the UI thread.
    pub fn insert_items(&mut self, ix: usize, items: Vec<MenuItem>, cx: &mut Context<Self>) {
        if items.is_empty() || ix > self.items.len() {
            return;
        }
        // Keyboard selection is an index into the same vector: shift it so the
        // highlight stays on the row the user picked.
        if let Some(selected) = self.selected.filter(|&s| s >= ix) {
            self.selected = Some(selected + items.len());
        }
        self.items.splice(ix..ix, items);
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(MenuItem::Action {
            handler,
            disabled: false,
            ..
        }) = self.selected.and_then(|ix| self.items.get(ix))
        {
            let handler = handler.clone();
            handler(window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.items.len() as isize;
        if len == 0 {
            return;
        }
        let mut ix = self.selected.map(|i| i as isize).unwrap_or(-delta);
        // Walk past separators/disabled items; bounded by one full wrap.
        for _ in 0..len {
            ix = (ix + delta).rem_euclid(len);
            if self.items[ix as usize].selectable() {
                self.selected = Some(ix as usize);
                return;
            }
        }
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
        let reserve_check_slot = self.items.iter().any(|item| {
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
            // Fires in the capture phase when the press lands outside our bounds.
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
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
            .children(self.items.iter().enumerate().map(|(ix, item)| match item {
                MenuItem::Separator => div().my_1().h(px(1.)).bg(colors.border).into_any_element(),
                MenuItem::Action {
                    label,
                    handler,
                    disabled,
                    checked,
                } => {
                    let handler = handler.clone();
                    let disabled = *disabled;
                    let checked = *checked;
                    let selected = self.selected == Some(ix);
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
            }))
    }
}
