//! The parts two pickers share: matching, and a row's highlighted label.
//!
//! Not a general picker component. Zed's `picker` is a delegate trait with
//! resizable geometry, preview panes, multi-select, and sqlite persistence,
//! and pane has two concrete lists that differ in where entries come from and
//! what choosing one does. Only the pieces that are genuinely identical live
//! here; the layout stays inline in each, where it reads.

use fuzzy_nucleo::{Case, LengthPenalty, StringMatch, StringMatchCandidate, match_strings};
use gpui::{HighlightStyle, SharedString, StyledText, TextStyle, Window};
use theme::ActiveTheme;

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
