//! The grid layout of a pane: cards in columns, a picture on each that has
//! one. Where they go is `crate::masonry`; this is the pane's side of it.
//!
//! The list's `uniform_list` scrolls itself. The grid is a `div` that scrolls
//! through gpui's interactivity layer, with its cards placed absolutely in a
//! child as tall as the layout, and only the cards near the view drawn.
//!
//! Three things the list does not need:
//!
//! - **The layout is cached.** Every card's height depends on its picture's
//!   shape, so laying out a listing asks the preview cache about every file.
//!   Scrolling changes none of that, so the layout is kept until the listing,
//!   the width, the card details, or something the preview cache learned has
//!   changed.
//! - **The view is anchored.** A card's shape is a guess from its extension
//!   until its facts arrive, and then the cards under it move. The first card
//!   in view keeps its place on screen across a new layout, so what moves is
//!   only what is out of sight.
//! - **A scroll can wait for a layout.** Selecting after a listing lands asks
//!   to reveal a card before the layout for that listing exists. The request
//!   waits in `pending` and is carried out by the render that has the layout.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::prelude::*;
use gpui::{Context, Window, div, point, px};
use theme::ActiveTheme as _;

use super::{Column, DirPane, INACTIVE_SELECTION_ALPHA, Reveal, quieted};
use crate::masonry::{GAP, Masonry};
use crate::preview::{self, Key, Need};

/// The card width the layout aims at. The column count follows the pane
/// width, and the cards stretch to fill each band.
const TARGET_WIDTH: f32 = 220.;
/// The name line under the picture.
const CAPTION_HEIGHT: f32 = 26.;
/// One line of card details.
const DETAIL_HEIGHT: f32 = 17.;
/// Below the last line.
const BOTTOM_PADDING: f32 = 5.;
/// A card's border, top and bottom together.
const BORDER: f32 = 2.;
/// Before the first layout there is no width to read. Close to a pane's, so
/// the first frame is near enough right; the next one measures.
const FIRST_GUESS: (f32, f32) = (900., 600.);
/// How far past the view the facts are fetched ahead, in viewports.
const LOOKAHEAD: f32 = 3.;

#[derive(Default)]
pub(super) struct GridState {
    pub(super) layout: Option<Masonry>,
    /// What the cached layout was made from.
    made_from: Option<Inputs>,
    /// The preview key of each entry, for this listing. `None` for an entry
    /// with no picture to ask for: a folder, an archive member.
    keys: Vec<Option<Key>>,
    keys_for: Option<u64>,
    /// The first card in view at the last render, and its top then.
    anchor: Option<(usize, f32)>,
    /// A scroll asked for before the layout it needs.
    pub(super) pending: Option<(usize, Reveal)>,
    /// The view's height at the last render.
    pub(super) viewport: f32,
}

#[derive(Clone, Copy, PartialEq)]
struct Inputs {
    listing: u64,
    width: f32,
    details: usize,
    learned: u64,
    scale: f32,
    previews: bool,
}

impl DirPane {
    pub(super) fn render_grid(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds = self.grid_scroll.bounds();
        let (width, height) = if bounds.size.width > px(0.) {
            (f32::from(bounds.size.width), f32::from(bounds.size.height))
        } else {
            FIRST_GUESS
        };
        let scale = window.scale_factor();
        let previews = preview::enabled(cx);
        // The scrollbar is laid over the right edge, so the cards stop short
        // of it rather than sit under it.
        self.grid_layout(width - crate::scrollbar::WIDTH, scale, previews, cx);

        let mut state = self.grid.borrow_mut();
        state.viewport = height;
        let layout = state.layout.clone().expect("laid out above");

        // A scroll that was waiting for this layout.
        if let Some((ix, how)) = state.pending.take()
            && let Some(offset) = reveal_offset(&layout, ix, how, self.grid_offset(), height)
        {
            self.set_grid_offset(offset);
        }

        let top = self.grid_offset();
        let near = layout.visible(top - height, top + 2. * height);
        let in_view = layout.visible(top, top + height);
        state.anchor = layout
            .first_in_view(top, top + height)
            .and_then(|ix| Some((ix, layout.top(ix)?)));

        // Pictures for the cards on screen first, then the margin round them,
        // then the facts of the cards a few screens on, so their shapes are
        // known before they are reached.
        let owner = cx.entity_id();
        let mut wants = Vec::new();
        let mut rank = 0u32;
        let mut push = |ix: usize, need: Need, base: u32, wants: &mut Vec<_>| {
            if let Some(Some(key)) = state.keys.get(ix) {
                wants.push((key.clone(), need, base + rank));
                rank += 1;
            }
        };
        for &ix in &in_view {
            push(ix, Need::Picture, 0, &mut wants);
        }
        for &ix in near.iter().filter(|ix| !in_view.contains(ix)) {
            push(ix, Need::Picture, 1_000_000, &mut wants);
        }
        let last = near.last().copied().unwrap_or(0);
        let ahead = ((near.len() as f32 * LOOKAHEAD) as usize).max(layout.columns() * 4);
        for ix in last + 1..(last + 1 + ahead).min(layout.len()) {
            push(ix, Need::Facts, 2_000_000, &mut wants);
        }
        let keys: Vec<Option<Key>> = near
            .iter()
            .map(|&ix| state.keys.get(ix).cloned().flatten())
            .collect();
        drop(state);
        preview::want(owner, wants, cx);

        let now = SystemTime::now();
        let cards: Vec<_> = near
            .iter()
            .zip(keys)
            .filter_map(|(&ix, key)| {
                let rect = layout.rect(ix)?;
                Some(self.render_card(ix, rect, key, now, cx))
            })
            .collect();

        let content = div()
            .relative()
            .w_full()
            .h(px(layout.total_height()))
            .children(cards);

        // `min_h_0` on both, or a flex item's minimum height is its content's
        // and the grid grows to the whole layout, pushing the footer out of
        // the window. The list's `uniform_list` measures itself and is spared.
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .relative()
            .child(
                div()
                    .id("grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.grid_scroll)
                    .child(content),
            )
            .child(crate::scrollbar::scrollbar(
                crate::scrollbar::ScrollSource::Pixels {
                    handle: self.grid_scroll.clone(),
                    content_height: layout.total_height(),
                },
                cx.entity(),
                |this: &mut Self| &mut this.scrollbar,
            ))
            .into_any_element()
    }

    /// Lay the cards out again if anything they depend on changed, and keep
    /// the first card in view where it was on screen.
    fn grid_layout(&self, width: f32, scale: f32, previews: bool, cx: &Context<Self>) {
        let inputs = Inputs {
            listing: self.listing_moved,
            width,
            details: self.view.card_details.iter().count(),
            learned: preview::learned(cx),
            scale,
            previews,
        };
        let mut state = self.grid.borrow_mut();
        if state.keys_for != Some(self.listing_moved)
            || state
                .made_from
                .is_some_and(|m| m.scale != scale || m.previews != previews)
        {
            state.keys = self
                .entries
                .iter()
                .map(|entry| {
                    let path = entry.on_disk().filter(|_| previews && !entry.is_dir)?;
                    Some(preview::key(
                        preview::Subject {
                            path: Arc::<Path>::from(path),
                            modified: entry.modified,
                            size: entry.size,
                        },
                        scale,
                    ))
                })
                .collect();
            state.keys_for = Some(self.listing_moved);
            // A new listing has nothing on screen to hold still.
            state.anchor = None;
        }
        if state.made_from == Some(inputs) && state.layout.is_some() {
            return;
        }

        let details = inputs.details as f32;
        let aspects: Vec<Option<f32>> = state
            .keys
            .iter()
            .map(|key| key.as_ref().and_then(|key| preview::card_aspect(key, cx)))
            .collect();
        let layout = Masonry::new(width, TARGET_WIDTH, self.entries.len(), |ix, card| {
            let picture = aspects[ix].map_or(0., |aspect| (card - BORDER) / aspect);
            BORDER + picture + CAPTION_HEIGHT + details * DETAIL_HEIGHT + BOTTOM_PADDING
        });

        // Anchoring: the card that was first in view moved by `shift`, so the
        // view moves with it.
        if let Some((ix, was)) = state.anchor
            && let Some(now) = layout.top(ix)
        {
            let shift = now - was;
            if shift.abs() > 0.5 {
                self.set_grid_offset(self.grid_offset() + shift);
            }
        }
        state.layout = Some(layout);
        state.made_from = Some(inputs);
    }

    /// How far down the grid is scrolled, in positive pixels.
    pub(super) fn grid_offset(&self) -> f32 {
        (-f32::from(self.grid_scroll.offset().y)).max(0.)
    }

    fn set_grid_offset(&self, offset: f32) {
        self.grid_scroll
            .set_offset(point(px(0.), px(-offset.max(0.))));
    }

    /// `scroll_to` in the grid: now if the layout is there, else at the next
    /// render.
    pub(super) fn grid_scroll_to(&self, ix: usize, how: Reveal) {
        self.grid.borrow_mut().pending = Some((ix, how));
    }

    /// The first card whose top is in view.
    pub(super) fn grid_top(&self) -> usize {
        let state = self.grid.borrow();
        let top = self.grid_offset();
        state
            .layout
            .as_ref()
            .and_then(|layout| layout.first_in_view(top, top + state.viewport))
            .unwrap_or(0)
    }

    /// Whether, with card `top` at the top of the view, card `ix` would be
    /// in view too.
    pub(super) fn grid_in_view(&self, top: usize, ix: usize) -> bool {
        let state = self.grid.borrow();
        let Some(layout) = state.layout.as_ref() else {
            return true;
        };
        match (layout.rect(top), layout.rect(ix)) {
            (Some((_, from, _, _)), Some((_, y, _, h))) => {
                y >= from && y + h <= from + state.viewport
            }
            _ => false,
        }
    }

    pub(super) fn grid_columns(&self) -> usize {
        self.grid
            .borrow()
            .layout
            .as_ref()
            .map_or(1, Masonry::columns)
    }

    /// Where a page key lands from `ix`: in the same column, a view away.
    pub(super) fn grid_page(&self, ix: usize, down: bool) -> Option<usize> {
        let state = self.grid.borrow();
        let page = (state.viewport - GAP).max(1.);
        state.layout.as_ref()?.page_from(ix, page, down)
    }

    /// The cards in view, as (index, x, y, width, height, picture) in window
    /// pixels, for the probe. `picture` is 1 once the card's picture is drawn.
    pub(super) fn grid_cards(&self, cx: &gpui::App) -> Vec<[i32; 6]> {
        let state = self.grid.borrow();
        let Some(layout) = state.layout.as_ref() else {
            return Vec::new();
        };
        let bounds = self.grid_scroll.bounds();
        let (left, top) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let offset = self.grid_offset();
        let keys = &state.keys;
        layout
            .visible(offset, offset + state.viewport)
            .into_iter()
            .take(crate::probe::MAX_ROWS)
            .filter_map(|ix| {
                let (x, y, w, h) = layout.rect(ix)?;
                Some([
                    ix as i32,
                    (left + x) as i32,
                    (top + y - offset) as i32,
                    w as i32,
                    h as i32,
                    keys.get(ix)
                        .and_then(Option::as_ref)
                        .is_some_and(|key| preview::has_picture(key, cx))
                        as i32,
                ])
            })
            .collect()
    }

    fn render_card(
        &self,
        ix: usize,
        (x, y, w, h): (f32, f32, f32, f32),
        key: Option<Key>,
        now: SystemTime,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let entry = &self.entries[ix];
        let selected = self.selected.contains(ix);
        let is_lead = self.cursor_ix == Some(ix);
        let colors = cx.theme().colors();
        let (content, name_color) = self.entry_colors(entry, cx);
        let muted = quieted(colors.text_muted, self.active);
        let icon = self.entry_icon(entry, cx);
        let label = entry.relative.clone().unwrap_or_else(|| entry.name.clone());
        let rename_editor = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.ix == ix)
            .map(|renaming| renaming.editor.clone());
        let picture = key
            .as_ref()
            .and_then(|key| Some((key, preview::card_aspect(key, cx)?)));
        let numeric = crate::theming::numeric_font(cx);
        let folder_bytes = self.folder_bytes(ix);
        let counting = self.counting_size(ix);
        let details: Vec<(Column, String)> = self
            .view
            .card_details
            .iter()
            .map(|column| (column, column.value(entry, folder_bytes, now)))
            .collect();

        let mut fill = colors.element_selected;
        if !self.active {
            fill.a *= INACTIVE_SELECTION_ALPHA;
        }
        let hover = colors.element_hover;

        let card = div()
            .id(ix)
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .h(px(h))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(if is_lead {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .bg(colors.elevated_surface_background)
            .cursor_pointer()
            .when(selected, |el| el.bg(fill))
            .when(!selected, |el| el.hover(move |style| style.bg(hover)))
            .when_some(picture, |el, (key, aspect)| {
                let width = w - BORDER;
                el.child(preview::card_picture(key, width, width / aspect, cx))
            })
            .child(
                div()
                    .h(px(CAPTION_HEIGHT))
                    .flex_none()
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .text_sm()
                    .text_color(name_color)
                    .children(icon.map(|path| super::Icon::from_path(path, content)))
                    .child(match rename_editor {
                        Some(editor) => gpui::IntoElement::into_any_element(editor),
                        None => gpui::IntoElement::into_any_element(
                            div().flex_1().min_w_0().truncate().child(label),
                        ),
                    }),
            )
            .children(details.into_iter().map(|(column, text)| {
                let line = div()
                    .h(px(DETAIL_HEIGHT))
                    .flex_none()
                    .px_2()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .when(column.is_numeric(), |el| {
                        el.when_some(numeric.clone(), |el, family| el.font_family(family))
                    });
                if counting && column == Column::Size {
                    line.child(super::counting_bar(content, ("card-counting", ix).into()))
                } else {
                    line.child(text)
                }
            }));
        self.interactive(card, ix, false, cx)
    }
}

/// Where to scroll to bring card `ix` into view as `how` says, or `None`
/// when no scroll is needed.
fn reveal_offset(
    layout: &Masonry,
    ix: usize,
    how: Reveal,
    offset: f32,
    viewport: f32,
) -> Option<f32> {
    let (_, y, _, h) = layout.rect(ix)?;
    let most = (layout.total_height() - viewport).max(0.);
    match how {
        Reveal::Nearest if y - GAP < offset => Some((y - GAP).max(0.)),
        Reveal::Nearest if y + h + GAP > offset + viewport => {
            Some((y + h + GAP - viewport).clamp(0., most))
        }
        Reveal::Nearest => None,
        Reveal::Top => Some((y - GAP).clamp(0., most)),
        Reveal::TopEvenPastTheEnd => Some((y - GAP).max(0.)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Masonry {
        // Three columns of cards 100 tall, bands every 108 from 8.
        Masonry::new(332., 100., 30, |_, _| 100.)
    }

    #[test]
    fn a_card_in_view_needs_no_scroll() {
        assert_eq!(reveal_offset(&layout(), 3, Reveal::Nearest, 0., 400.), None);
    }

    #[test]
    fn a_card_below_scrolls_up_to_its_bottom() {
        // Card 12 is in band 4: top 440, bottom 540.
        let offset = reveal_offset(&layout(), 12, Reveal::Nearest, 0., 400.).unwrap();
        assert_eq!(offset, 540. + GAP - 400.);
    }

    #[test]
    fn a_card_above_scrolls_down_to_its_top() {
        let offset = reveal_offset(&layout(), 3, Reveal::Nearest, 500., 400.).unwrap();
        assert_eq!(offset, 116. - GAP);
    }

    #[test]
    fn top_stops_at_the_end_unless_told_not_to() {
        let layout = layout();
        let most = layout.total_height() - 400.;
        assert_eq!(
            reveal_offset(&layout, 29, Reveal::Top, 0., 400.),
            Some(most)
        );
        let past = reveal_offset(&layout, 29, Reveal::TopEvenPastTheEnd, 0., 400.).unwrap();
        assert!(past > most);
    }
}
