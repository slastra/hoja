//! What the command palette and the place finder share: the query field, the
//! match list, keyboard navigation, and the modal itself.
//!
//! Still not a general picker component. Zed's `picker` is a delegate trait
//! with resizable geometry, preview panes, multi-select, and sqlite
//! persistence. This is the concrete overlap of two concrete lists, they
//! differ only in where entries come from, what a row looks like, and what
//! choosing one does, so that is all either of them defines.
//!
//! An earlier attempt kept only matching here and left "the layout inline in
//! each, where it reads". The layout was not per-picker; it was the same
//! sixty-five lines twice, and it had already drifted.

use fuzzy_nucleo::{Case, LengthPenalty, StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    AnyElement, App, BoxShadow, Div, Entity, HighlightStyle, Pixels, SharedString, StyledText,
    TextStyle, UniformList, UniformListScrollHandle, Window, actions, div, hsla, prelude::*, px,
};
use theme::ActiveTheme;

use crate::path_editor::PathEditor;
use crate::scrollbar::{ScrollSource, ScrollbarState, scrollbar};

// Owned here, not by one of the two pickers: which keys move a picker's
// selection is picker business, and having the finder reach into the palette
// for them is how `places::Toggle` ended up listed as a command while
// `palette::Toggle` was hidden.
actions!(picker, [SelectNext, SelectPrevious]);

/// The key context both modals declare, so one set of bindings serves both.
pub const KEY_CONTEXT: &str = "picker";

/// Beyond this the list scrolls rather than growing, so a modal never runs off
/// the bottom of the window.
const MAX_VISIBLE_ROWS: f32 = 10.;

/// Space above and below the rows, as Zed's picker has (`py_1`).
const LIST_PADDING: f32 = 4.;

/// The modal's width: Zed's command palette is 38 rems.
const WIDTH: f32 = 608.;

/// The query field, the matches, and where the highlight sits.
pub struct PickerState {
    pub query: Entity<PathEditor>,
    pub candidates: Vec<StringMatchCandidate>,
    pub matches: Vec<StringMatch>,
    pub selected: usize,
    pub scroll: UniformListScrollHandle,
    pub scrollbar: ScrollbarState,
}

impl PickerState {
    pub fn new(query: Entity<PathEditor>) -> Self {
        Self {
            query,
            candidates: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            scrollbar: ScrollbarState::default(),
        }
    }

    /// Re-run the match and put the highlight back on the first row.
    pub fn rematch(&mut self, query: &str) {
        self.matches = match_entries(&self.candidates, query);
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
    }

    /// Re-run the match, but leave the highlight on the entry it is already on.
    ///
    /// For a list that grows *underneath* the user rather than one they
    /// retyped: the place finder splices in volumes when a slow `lsblk`
    /// returns, and a plain `rematch` would send the highlight back to the top,
    /// moving it out from under an enter the user is in the middle of
    /// pressing.
    pub fn rematch_keeping_selection(&mut self, query: &str) {
        let held = self.matches.get(self.selected).map(|m| m.candidate_id);
        self.rematch(query);
        if let Some(ix) = held.and_then(|id| self.matches.iter().position(|m| m.candidate_id == id))
        {
            self.selected = ix;
            self.scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        }
    }

    /// Wrapping: a picker list is short, and holding a key down should come
    /// back around rather than stick at the end.
    pub fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Nearest);
    }

    /// The item behind the highlighted row, given the caller's own list.
    pub fn chosen<'a, T>(&self, items: &'a [T]) -> Option<(&'a T, &StringMatch)> {
        let matched = self.matches.get(self.selected)?;
        Some((items.get(matched.candidate_id)?, matched))
    }
}

/// The height a list of these matches must be given.
///
/// It has to go on the `uniform_list` itself, never on a container around it.
/// The list virtualizes, so it builds however many rows fit *its own* height; a
/// definite height on a parent tells it nothing. A list with no height of its
/// own therefore draws no rows at all while the parent still reserves the
/// space, which reads as an empty modal rather than as a layout mistake.
fn list_height(state: &PickerState, row_height: f32) -> Pixels {
    px(row_height * (state.matches.len() as f32).min(MAX_VISIBLE_ROWS) + 2. * LIST_PADDING)
}

/// The rows, sized, padded and given a scrollbar laid over them, as Zed's
/// picker has one. `owner` is the picker entity, and `bar` finds its
/// `PickerState::scrollbar`.
pub fn results<T: 'static>(
    state: &PickerState,
    list: UniformList,
    row_height: f32,
    owner: Entity<T>,
    bar: fn(&mut T) -> &mut ScrollbarState,
) -> AnyElement {
    let overflows = state.matches.len() as f32 > MAX_VISIBLE_ROWS;
    div()
        .relative()
        .flex()
        .flex_col()
        .child(
            list.track_scroll(&state.scroll)
                .h(list_height(state, row_height))
                .py(px(LIST_PADDING))
                // Room for the bar while there is anything to scroll. Zed's
                // lies over the rows and fades a second after the scroll;
                // hoja's stays, as in the listing, so it must not cover the
                // key bindings at the rows' right.
                .when(overflows, |list| list.pr(px(crate::scrollbar::WIDTH))),
        )
        .child(scrollbar(
            ScrollSource::Rows {
                handle: state.scroll.clone(),
                rows: state.matches.len(),
                row_height,
            },
            owner,
            bar,
        ))
        .into_any_element()
}

/// Zed's modal elevation (`elevation_3`): its surface, the variant border,
/// and the layered shadow it gives a modal.
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

/// The modal, laid out as Zed's picker is: the query on a 36 pixel row with a
/// rule under it, then the rows, or a muted line when nothing matches, then
/// the footer when there is one.
///
/// The list arrives from `results`, already sized: see `list_height`.
pub fn shell(
    state: &PickerState,
    empty_message: &'static str,
    list: AnyElement,
    footer: Option<AnyElement>,
    cx: &App,
) -> Div {
    let colors = cx.theme().colors();
    let count = state.matches.len();

    div()
        .occlude()
        .flex()
        .flex_col()
        .w(px(WIDTH))
        .rounded_lg()
        .border_1()
        .border_color(colors.border_variant)
        .bg(colors.elevated_surface_background)
        .shadow(modal_shadow(cx))
        .child(
            div()
                .flex_none()
                .h_9()
                .px(px(10.))
                .flex()
                .items_center()
                .border_b_1()
                .border_color(colors.border_variant)
                .text_sm()
                .child(state.query.clone()),
        )
        .when(count == 0, |el| {
            // Keeps the modal from collapsing and jumping when nothing
            // matches. Placed as a row's text is, as Zed places it.
            el.child(
                div().py_2().px_1().child(
                    div()
                        .px(px(6.))
                        .py_1()
                        .text_sm()
                        .text_color(colors.text_muted)
                        .child(empty_message),
                ),
            )
        })
        .when(count > 0, |el| el.child(list))
        .children(footer.map(|footer| {
            div()
                .flex_none()
                .p_1p5()
                .flex()
                .flex_row()
                .justify_end()
                .gap_1()
                .border_t_1()
                .border_color(colors.border_variant)
                .child(footer)
        }))
}

/// A row, as Zed's inset list item: rounded, set in from the modal's sides,
/// filled while selected, and lit under the pointer without moving the
/// selection. The caller fills in what it says, then hands it to `inset`.
pub fn row(ix: usize, row_height: f32, selected: bool, cx: &App) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    div()
        .id(ix)
        .h(px(row_height))
        .w_full()
        .px(px(6.))
        .rounded_sm()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .text_sm()
        .text_color(colors.text)
        .when(selected, |row| row.bg(colors.ghost_element_selected))
        .when(!selected, |row| {
            row.hover(|style| style.bg(colors.ghost_element_hover))
        })
        .active(|style| style.bg(colors.ghost_element_active))
}

/// A finished `row`, set in from the modal's sides.
pub fn inset(row: gpui::Stateful<Div>) -> AnyElement {
    div().w_full().px_1().child(row).into_any_element()
}

/// A key binding as Zed writes one on Linux: muted text, modifiers first,
/// each word capitalised, joined by hyphens. `Ctrl-Shift-P`, not keycaps.
pub fn key_text(keystroke: &gpui::KeybindingKeystroke) -> String {
    let modifiers = keystroke.modifiers();
    let mut text = String::new();
    for (on, name) in [
        (modifiers.function, "Fn"),
        (modifiers.control, "Ctrl"),
        (modifiers.platform, "Super"),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
    ] {
        if on {
            text.push_str(name);
            text.push('-');
        }
    }
    text.push_str(&key_name(keystroke.key()));
    text
}

fn key_name(key: &str) -> String {
    match key {
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        key => {
            let mut chars = key.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// The key bindings of one action, in a row: each chord as `key_text`.
pub fn key_binding(chords: &[String], cx: &App) -> Div {
    let muted = cx.theme().colors().text_muted;
    div()
        .flex_none()
        .flex()
        .flex_row()
        .gap_1()
        .text_sm()
        .text_color(muted)
        .children(chords.iter().map(|chord| div().child(chord.clone())))
}

/// Match `query` against `candidates`, or pass everything through in order when
/// the query is empty.
///
/// Matching is synchronous. Zed runs it on a background executor because it
/// matches thousands of actions; pane matches dozens, so the pending-update and
/// cancellation machinery buys nothing.
pub fn match_entries(candidates: &[StringMatchCandidate], query: &str) -> Vec<StringMatch> {
    if query.is_empty() {
        return candidates
            .iter()
            .map(|candidate| StringMatch {
                candidate_id: candidate.id,
                score: 0.,
                positions: Vec::new(),
                string: candidate.string.clone(),
            })
            .collect();
    }
    match_strings(
        candidates,
        query,
        // Smart case: match case-insensitively and let case only score. Zed
        // documented this the hard way: matching case-sensitively rejects a
        // capitalised query against a lower-case name, which is the common case.
        Case::smart_if_uppercase_in(query),
        LengthPenalty::On,
        usize::MAX,
    )
}

/// A label with the matched characters in the accent colour.
///
/// `with_default_highlights` takes the style it is given rather than inheriting
/// from the element tree, so the colour has to be set here, a `text_color` on
/// an ancestor is silently ignored.
///
/// The offsets are BYTES into the string that was *matched*, which is not
/// always the string being drawn: a candidate may combine a label with a detail
/// so that both are searchable, while only the label is highlighted. Ranges
/// outside `text`, or landing inside a multi-byte character, are dropped:
/// gpui asserts on a char boundary and takes the process with it, so clipping
/// belongs here rather than in every caller.
pub fn highlighted_label(
    text: impl Into<SharedString>,
    matched: &StringMatch,
    window: &Window,
    cx: &gpui::App,
) -> StyledText {
    let colors = cx.theme().colors();
    let mut style: TextStyle = window.text_style();
    style.color = colors.text;

    let text = text.into();
    let highlights: Vec<_> = matched
        .ranges()
        .filter(|range| {
            range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end)
        })
        .map(|range| {
            (
                range,
                HighlightStyle {
                    color: Some(colors.text_accent),
                    ..Default::default()
                },
            )
        })
        .collect();

    StyledText::new(text).with_default_highlights(&style, highlights)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(keys: &'static str) -> String {
        let keystroke = gpui::Keystroke::parse(keys).unwrap();
        key_text(&gpui::KeybindingKeystroke::from_keystroke(keystroke))
    }

    #[test]
    fn keys_are_written_as_zed_writes_them_on_linux() {
        assert_eq!(text("ctrl-shift-p"), "Ctrl-Shift-P");
        assert_eq!(text("f2"), "F2");
        assert_eq!(text("alt-left"), "Alt-Left");
        assert_eq!(text("ctrl-pagedown"), "Ctrl-PageDown");
        assert_eq!(text("super-escape"), "Super-Escape");
        assert_eq!(text("ctrl-1"), "Ctrl-1");
    }
}
