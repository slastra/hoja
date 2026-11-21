//! What frames the listing: the toolbar above it and the footer below.

use super::*;

/// The footer line on screen: its parts with a thin vertical rule between
/// them, as Zed's status bar separates its items, where the text has " · ".
///
/// The line itself keeps the dots, because the probe reads it and a test
/// asserts on what a person reads. Split on the separator here rather than
/// built twice, so the two cannot disagree about what the parts are.
pub(super) fn footer_segments(line: &str, rule: gpui::Hsla) -> impl IntoElement + use<> {
    let mut row = div().flex().flex_row().items_center().gap_2().min_w_0();
    for (ix, part) in line.split(" · ").enumerate() {
        if ix > 0 {
            row = row.child(div().flex_none().w(px(1.)).h(px(12.)).bg(rule));
        }
        row = row.child(div().truncate().child(part.to_string()));
    }
    row
}

impl DirPane {
    /// Navigation toolbar: back / forward / up / home buttons plus the path.
    pub(super) fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        // The inactive pane's path drops to the muted colour, as Zed dims the
        // tab and breadcrumb of a pane that is not the focused one.
        let content = if self.active {
            colors.text
        } else {
            colors.text_muted
        };
        // Back, forward, up and home say exactly one thing: whether there is
        // anywhere to go. Dimming them with the pane as well would put two
        // meanings in one colour, and a disabled Back would then look identical
        // to a Back in a pane that simply is not focused.
        let nav_on = colors.text;
        let nav_off = colors.text_muted;
        let hover_bg = colors.element_hover;
        let searching = self.searching();

        let nav_button = |id: &'static str,
                          icon: &'static str,
                          enabled: bool,
                          action: Box<dyn gpui::Action>,
                          cx: &Context<Self>| {
            div()
                .id(id)
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .child(Icon::from_path(
                    icon,
                    if enabled { nav_on } else { nav_off },
                ))
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|s| s.bg(hover_bg))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            window.focus(&this.focus_handle, cx);
                            window.dispatch_action(action.boxed_clone(), cx);
                        }))
                })
        };

        // The rules either side of the path, the toolbar's full height, so
        // they meet the rule under it.
        let rule = || {
            div()
                .flex_none()
                .self_stretch()
                .w(px(1.))
                .mx_1()
                .bg(colors.border)
        };

        div()
            .flex_none()
            .h(px(28.))
            .flex()
            .flex_row()
            .items_center()
            .gap_0p5()
            .px_1()
            .bg(colors.title_bar_background)
            // The rule under the toolbar, in both layouts: the column header
            // under it in the list draws only its own lower rule.
            .border_b_1()
            .border_color(colors.border)
            .text_sm()
            .child(nav_button(
                "nav-back",
                "icons/file_icons/arrow_left.svg",
                self.history.can_back(),
                Box::new(NavBack),
                cx,
            ))
            .child(nav_button(
                "nav-forward",
                "icons/file_icons/arrow_right.svg",
                self.history.can_forward(),
                Box::new(NavForward),
                cx,
            ))
            .child(nav_button(
                "nav-up",
                "icons/file_icons/arrow_up.svg",
                self.dir.parent().is_some(),
                Box::new(GoUp),
                cx,
            ))
            .child(nav_button(
                "nav-home",
                "icons/file_icons/house.svg",
                std::env::var_os("HOME").map(PathBuf::from).as_deref() != self.dir.disk(),
                Box::new(GoHome),
                cx,
            ))
            .child(rule())
            .child(match self.path_editor.clone() {
                // The label's padding, so the path does not move when it
                // becomes editable.
                Some((_, editor)) => div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .flex()
                    .child(editor)
                    .into_any_element(),
                None => div()
                    .id("path-display-slot")
                    .flex_1()
                    .px_2()
                    .truncate()
                    .text_color(content)
                    .cursor_text()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.start_edit(window, cx);
                    }))
                    .child(self.dir.to_string())
                    .into_any_element(),
            })
            .child(rule())
            .child(
                div()
                    .id("hoja-search")
                    .flex_none()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    // Lit while a search is on, since the listing is then
                    // results rather than this directory and the toolbar is the
                    // only thing that says so.
                    .when(searching, |el| el.bg(colors.element_selected))
                    .hover(|s| s.bg(hover_bg))
                    .child(Icon::from_path(
                        "icons/file_icons/magnifying_glass.svg",
                        content,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            // A second press ends the search, matching escape.
                            if this.searching() {
                                this.end_search(window, cx);
                            } else {
                                this.start_search(&StartSearch, window, cx);
                            }
                        }),
                    ),
            )
            // The two layouts, as a pair of buttons that light the one in
            // force, the way most file managers offer them.
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .children(
                        [
                            (
                                "layout-list",
                                "icons/file_icons/layout_list.svg",
                                fs::Layout::List,
                            ),
                            (
                                "layout-grid",
                                "icons/file_icons/layout_grid.svg",
                                fs::Layout::Grid,
                            ),
                        ]
                        .into_iter()
                        .map(|(id, icon, layout)| {
                            let on = self.view.layout == layout;
                            div()
                                .id(id)
                                .size(px(20.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .when(on, |el| el.bg(colors.element_selected))
                                .when(!on, |el| el.hover(|s| s.bg(hover_bg)))
                                .child(Icon::from_path(icon, content))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.set_layout(layout, cx);
                                }))
                        }),
                    ),
            )
            .child(
                div()
                    .id("pane-menu")
                    .flex_none()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg))
                    .child(Icon::from_path("icons/file_icons/menu.svg", content))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_settings_menu(event.position, window, cx);
                        }),
                    ),
            )
    }

    /// The line at the bottom of the pane: what is selected, what the directory
    /// holds, or what a search is doing.
    pub(super) fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .px_2()
            .bg(colors.title_bar_background)
            .border_t_1()
            .border_color(colors.border)
            .text_xs()
            // Muted in both panes, like the column headers and for the same
            // reason: this is furniture. It says the same kind of thing every
            // time you look, and must not compete with the names above it.
            .text_color(colors.text_muted)
            // The same face as the Size column it totals, so "259 MB" here and
            // "259 MB" in a row above are the same width and the same shape.
            .when_some(crate::theming::numeric_font(cx), |el, family| {
                el.font_family(family)
            })
            // A search is the pane's loudest running work, and while one is on
            // the rows are not this directory's, so nothing the footer would
            // otherwise say about them is true.
            .child(footer_segments(&self.footer_line(), colors.border))
    }
}
