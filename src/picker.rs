//! What the command palette and the place finder share: the query field, the
//! match list, keyboard navigation, and the modal itself.
//!
//! Still not a general picker component. Zed's `picker` is a delegate trait
//! with resizable geometry, preview panes, multi-select, and sqlite
//! persistence. This is the concrete overlap of two concrete lists — they
//! differ only in where entries come from, what a row looks like, and what
//! choosing one does, so that is all either of them defines.
//!
//! An earlier attempt kept only matching here and left "the layout inline in
//! each, where it reads". The layout was not per-picker; it was the same
//! sixty-five lines twice, and it had already drifted.

use fuzzy_nucleo::{Case, LengthPenalty, StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    AnyElement, App, Div, Entity, HighlightStyle, SharedString, StyledText, TextStyle,
    UniformListScrollHandle, Window, actions, div, prelude::*, px,
};
use theme::ActiveTheme;

use crate::path_editor::PathEditor;

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

/// The query field, the matches, and where the highlight sits.
pub struct PickerState {
    pub query: Entity<PathEditor>,
    pub candidates: Vec<StringMatchCandidate>,
    pub matches: Vec<StringMatch>,
    pub selected: usize,
    pub scroll: UniformListScrollHandle,
}

impl PickerState {
    pub fn new(query: Entity<PathEditor>) -> Self {
        Self {
            query,
            candidates: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Re-run the match and put the highlight back on the first row.
    pub fn rematch(&mut self, query: &str) {
        self.matches = match_entries(&self.candidates, query);
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
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

/// The modal: query field on top, rows or an empty message below.
///
/// `row_height` is the caller's, because a command row and a place row are
/// different heights, and the list must be sized from it.
pub fn shell(
    state: &PickerState,
    row_height: f32,
    empty_message: &'static str,
    list: AnyElement,
    cx: &App,
) -> Div {
    let colors = cx.theme().colors();
    let count = state.matches.len();

    div()
        .occlude()
        .flex()
        .flex_col()
        .w(px(544.))
        .rounded_lg()
        .border_1()
        .border_color(colors.border)
        .bg(colors.elevated_surface_background)
        .shadow_lg()
        .child(
            div()
                .flex_none()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(colors.border)
                .text_sm()
                .child(state.query.clone()),
        )
        .when(count == 0, |el| {
            // Keeps the modal from collapsing and jumping when nothing matches.
            el.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(empty_message),
            )
        })
        .when(count > 0, |el| {
            // uniform_list virtualizes, so it needs a definite height rather
            // than a maximum: given only a max it renders nothing.
            el.child(div().h(px(row_height * (count as f32).min(MAX_VISIBLE_ROWS))).child(list))
        })
}

/// A row's chrome. The caller fills in what the row says.
pub fn row(ix: usize, row_height: f32, selected: bool, cx: &App) -> gpui::Stateful<Div> {
    let colors = cx.theme().colors();
    div()
        .id(ix)
        .h(px(row_height))
        .w_full()
        .px_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .text_sm()
        .text_color(colors.text)
        .when(selected, |row| row.bg(colors.element_selected))
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
        // documented this the hard way — matching case-sensitively rejects a
        // capitalised query against a lower-case name, which is the common case.
        Case::smart_if_uppercase_in(query),
        LengthPenalty::On,
        usize::MAX,
    )
}

/// A label with the matched characters in the accent colour.
///
/// `with_default_highlights` takes the style it is given rather than inheriting
/// from the element tree, so the colour has to be set here — a `text_color` on
/// an ancestor is silently ignored.
///
/// The offsets are BYTES into the string that was *matched*, which is not
/// always the string being drawn: a candidate may combine a label with a detail
/// so that both are searchable, while only the label is highlighted. Ranges
/// outside `text`, or landing inside a multi-byte character, are dropped —
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
