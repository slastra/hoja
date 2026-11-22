//! Controls drawn as Zed's `ui` crate draws them.
//!
//! hoja does not depend on that crate: it pulls in most of the editor. These
//! are the few pieces hoja needs, with the sizes and theme colors copied from
//! Zed at the pinned revision (`crates/ui/src/components`). Keep them in step
//! with it rather than adjusting them here one at a time.

use gpui::{
    AnyElement, AnyView, App, BoxShadow, Context, Div, ElementId, Hsla, SharedString, Stateful,
    Styled, Window, div, hsla, prelude::*, px, relative,
};
use theme::ActiveTheme;

use crate::icon::Icon;

// The keys of every prompt. A context of their own, so menu bindings cannot
// leak into a prompt, and so a change to the menu keys cannot change what a
// prompt does.
gpui::actions!(dialog, [Cancel, Confirm, SelectNext, SelectPrevious]);

/// The context the actions above are bound in.
pub const PROMPT_CONTEXT: &str = "dialog";

/// A button's look. Zed's `ButtonStyle`, less the variants hoja has no use for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// No fill until the pointer is on it. Zed's default.
    Subtle,
    /// A hairline round it, on the surface it sits on.
    Outlined,
    /// Filled with a status color, to mark the action Enter takes.
    Accent,
}

/// Zed's `ButtonSize::Default`: 22 pixels high, 4 pixels each side.
const BUTTON_HEIGHT: f32 = 22.;

/// A button's frame: Zed's `ButtonLike`. The caller adds the label, an icon
/// or both, and what a click does.
pub fn button(id: impl Into<ElementId>, style: ButtonStyle, cx: &App) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let status = cx.theme().status();
    let (background, border, hover, active) = match style {
        ButtonStyle::Subtle => (
            colors.ghost_element_background,
            None,
            colors.ghost_element_hover,
            colors.ghost_element_active,
        ),
        // Zed fills it by the elevation it is told it sits on, and with none
        // given, as in a prompt, that is the element background.
        ButtonStyle::Outlined => (
            colors.element_background,
            Some((colors.border_variant, colors.border)),
            colors.ghost_element_hover,
            colors.element_active,
        ),
        ButtonStyle::Accent => {
            let hover = cx.theme().darken(status.info_background, 0.05, 0.2);
            (
                status.info_background,
                Some((status.info_border, status.info_border)),
                hover,
                status.info_background,
            )
        }
    };
    div()
        .id(id)
        .flex_none()
        .h(px(BUTTON_HEIGHT))
        .px_1()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .rounded_sm()
        .text_sm()
        .text_color(colors.text)
        .cursor_pointer()
        .bg(background)
        .when_some(border, |el, (border, _)| el.border_1().border_color(border))
        // One `hover` per element: gpui panics on a second.
        .hover(move |s| {
            let s = s.bg(hover);
            match border {
                Some((_, hover_border)) => s.border_color(hover_border),
                None => s,
            }
        })
        .active(move |s| s.bg(active))
}

/// A button holding only text.
pub fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    style: ButtonStyle,
    cx: &App,
) -> Stateful<Div> {
    button(id, style, cx).child(label.into())
}

/// Zed's `IconButton` at `IconSize::Small`: a 14 pixel icon in a square of
/// 18, muted until something says otherwise.
pub fn icon_button(id: impl Into<ElementId>, icon: &'static str, cx: &App) -> Stateful<Div> {
    icon_button_colored(id, icon, cx.theme().colors().text_muted, cx)
}

pub fn icon_button_colored(
    id: impl Into<ElementId>,
    icon: &'static str,
    color: Hsla,
    cx: &App,
) -> Stateful<Div> {
    button(id, ButtonStyle::Subtle, cx)
        .size(px(18.))
        .px_0()
        .justify_center()
        .child(Icon::from_path(icon, color).size(px(14.)))
}

/// Zed's `Checkbox`: a 16 pixel box set in a square of 20, then its label in
/// muted text. The caller gives it its click.
pub fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = cx.theme().colors();
    let border = colors.border;
    let group: SharedString = "checkbox".into();
    div()
        .id(id)
        .group(group.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .size(px(20.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex_none()
                        .m_0p5()
                        .size_4()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_xs()
                        .border_1()
                        .border_color(border)
                        .group_hover(group, move |s| s.border_color(border.alpha(0.7)))
                        .when(checked, |el| {
                            el.child(
                                Icon::from_path("icons/file_icons/check.svg", colors.text_accent)
                                    .size(px(14.)),
                            )
                        }),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(colors.text_muted)
                .child(label.into()),
        )
}

/// Zed's `ProgressBar`: a rounded track with the done part in the info
/// color. Never drawn empty: Zed keeps two percent of it lit, so a bar that
/// has started reads as a bar.
///
/// `track` is the caller's, as Zed lets a caller set it. Zed's default is the
/// window background, which on a status bar is the bar's own color, so the
/// track vanished and two percent of a transfer read as a dash.
pub fn progress_bar(
    id: impl Into<ElementId>,
    fraction: f32,
    track: Hsla,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h_2()
        .p_0p5()
        .rounded_full()
        .bg(track)
        .shadow(vec![BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.08))])
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(cx.theme().status().info)
                .w(relative(fraction.clamp(0.02, 1.))),
        )
}

/// Zed's `elevation_3`, the level a modal sits at: its surface, the variant
/// border and a layered shadow.
pub fn elevation_3(el: Div, cx: &App) -> Div {
    let colors = cx.theme().colors();
    el.rounded_lg()
        .border_1()
        .border_color(colors.border_variant)
        .bg(colors.elevated_surface_background)
        .shadow(modal_shadow(cx))
}

fn modal_shadow(cx: &App) -> Vec<BoxShadow> {
    let light = cx.theme().appearance == theme::Appearance::Light;
    let black = |alpha: f32| hsla(0., 0., 0., alpha);
    vec![
        BoxShadow::new(px(0.), px(2.), black(if light { 0.06 } else { 0.12 })).blur_radius(px(3.)),
        BoxShadow::new(px(0.), px(3.), black(if light { 0.06 } else { 0.08 })).blur_radius(px(6.)),
        BoxShadow::new(px(0.), px(6.), black(0.04)).blur_radius(px(12.)),
        BoxShadow::new(px(0.), px(1.), black(if light { 0.04 } else { 0.12 })),
    ]
}

/// Zed's `Tooltip`: a line of text on the raised surface (`elevation_2`),
/// set down and right of the pointer so the pointer does not cover it.
/// For a button whose icon alone could be read two ways.
pub fn tooltip(title: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let title = title.into();
    move |_, cx| {
        cx.new(|_| Tooltip {
            title: title.clone(),
        })
        .into()
    }
}

struct Tooltip {
    title: SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let light = cx.theme().appearance == theme::Appearance::Light;
        div().pl_2().pt_2p5().child(
            div()
                .rounded_lg()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.elevated_surface_background)
                .shadow(vec![
                    BoxShadow::new(px(0.), px(2.), hsla(0., 0., 0., 0.12)).blur_radius(px(3.)),
                    BoxShadow::new(
                        px(0.),
                        px(1.),
                        hsla(0., 0., 0., if light { 0.03 } else { 0.06 }),
                    ),
                ])
                .py_1()
                .px_2()
                .text_sm()
                .text_color(colors.text)
                .max_w_72()
                .child(self.title.clone()),
        )
    }
}

/// Zed's prompt, as it draws one where the system has none to offer: 320
/// pixels wide, the question, a muted line under it, then the answers stacked
/// full width with the one Enter takes in the accent.
///
/// `extra` goes between the detail and the answers. The caller wraps the
/// result in its focus and its actions; `prompt_scrim` centers it.
pub fn prompt(
    message: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    extra: Option<AnyElement>,
    answers: Vec<AnyElement>,
    cx: &App,
) -> Div {
    let colors = cx.theme().colors();
    elevation_3(div(), cx)
        .occlude()
        .flex()
        .flex_col()
        .w_80()
        .p_4()
        .gap_4()
        .overflow_hidden()
        .cursor_default()
        .child(
            div()
                .w_full()
                .text_sm()
                .text_color(colors.text)
                .child(message.into()),
        )
        .child(
            div()
                .w_full()
                .text_xs()
                .text_color(colors.text_muted)
                .child(detail.into()),
        )
        .children(extra)
        .child(div().flex().flex_col().gap_1().children(answers))
}

/// One answer in a `prompt`. `active` is the one Enter takes, which the
/// arrow keys and Tab move.
pub fn prompt_answer(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let style = if active {
        ButtonStyle::Accent
    } else {
        ButtonStyle::Outlined
    };
    text_button(id, label, style, cx).w_full().justify_center()
}

/// The layer under a prompt: Zed's, a light shade over the whole window with
/// the prompt in its center.
pub fn prompt_scrim(prompt: AnyElement) -> Div {
    div()
        .occlude()
        .absolute()
        .inset_0()
        .bg(hsla(0., 0., 0., 0.2))
        .flex()
        .items_center()
        .justify_center()
        .child(prompt)
}

/// The answer after `active` among `count`, or before it, wrapping round as
/// Zed's prompt does.
pub fn step_answer(active: usize, count: usize, forward: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if forward {
        (active + 1) % count
    } else {
        (active + count - 1) % count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_answer_wraps_both_ways() {
        assert_eq!(step_answer(0, 4, true), 1);
        assert_eq!(step_answer(3, 4, true), 0);
        assert_eq!(step_answer(0, 4, false), 3);
        assert_eq!(step_answer(0, 0, true), 0);
    }
}
