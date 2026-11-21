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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::prelude::*;
use gpui::{Context, Window, div, point, px};
use theme::ActiveTheme as _;

use super::{Column, DirPane, INACTIVE_SELECTION_ALPHA, Reveal, quieted};
use crate::masonry::{GAP, Masonry};
use crate::preview::{self, Key, Need};

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
    /// The same keys without the gaps, in order, for the loader's
    /// background sweep of shapes.
    sweep: Option<Arc<Vec<Key>>>,
    /// The shape each card was laid out with, so shapes can be held.
    aspects: Vec<Option<f32>>,
    /// Each card's settled shape, `None` while it is still a guess, and its
    /// guess from the extension. Kept here and brought up to date from the
    /// preview cache's change log, so a layout looks at the few cards that
    /// learned something rather than asking about every one.
    settled: Vec<Option<Option<f32>>>,
    guesses: Vec<Option<f32>>,
    index_of: HashMap<Key, usize>,
    /// Where in the change log this grid has read to; `None` before the
    /// first look at a listing, which looks at every card.
    seq: Option<u64>,
    /// Cards settled while above the view, still laid out at their old shape.
    held: Vec<usize>,
    /// The first card in view at the last render, and its top then.
    anchor: Option<(usize, f32)>,
    /// A scroll asked for before the layout it needs.
    pub(super) pending: Option<(usize, Reveal)>,
    /// The view's height at the last render.
    pub(super) viewport: f32,
    /// Whether the layout was made from the grid's own measured width,
    /// rather than borrowed or guessed before its first frame.
    pub(super) measured: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Inputs {
    listing: u64,
    width: f32,
    /// The card width aimed at: the zoom.
    target: u16,
    details: usize,
    /// Where the preview cache's change log ended: it moves when anything
    /// is learned.
    log: u64,
    scale: f32,
    previews: bool,
}

impl GridState {
    /// A new listing's keys. Everything worked out from the old ones starts
    /// over, and with nothing yet on screen there is nothing to hold still.
    fn reset_keys(&mut self, keys: Vec<Option<Key>>, listing: u64) {
        self.guesses = keys
            .iter()
            .map(|key| key.as_ref().and_then(preview::guessed_aspect))
            .collect();
        self.index_of = keys
            .iter()
            .enumerate()
            .filter_map(|(ix, key)| Some((key.clone()?, ix)))
            .collect();
        self.sweep = Some(Arc::new(keys.iter().flatten().cloned().collect()));
        self.settled = vec![None; keys.len()];
        self.keys = keys;
        self.keys_for = Some(listing);
        self.seq = None;
        self.held.clear();
        self.aspects.clear();
        self.anchor = None;
    }

    /// The cards whose shapes may have changed: the ones the change log
    /// names since this grid last read it, or every card when it has not read
    /// it yet or the log no longer reaches back that far. `log` is that
    /// reading; `log_end` is where the log ends, taken before looking, so a
    /// change made while looking is not lost.
    fn changed_cards(&mut self, log: Option<(Vec<Key>, u64)>, log_end: u64) -> Vec<usize> {
        match log {
            Some((keys, end)) => {
                self.seq = Some(end);
                keys.iter()
                    .filter_map(|key| self.index_of.get(key).copied())
                    .collect()
            }
            None => {
                self.seq = Some(log_end);
                (0..self.keys.len()).collect()
            }
        }
    }

    /// Record the settled shapes of `cards`, from `settled`.
    fn settle(&mut self, cards: &[usize], settled: impl Fn(&Key) -> Option<Option<f32>>) {
        for &ix in cards {
            self.settled[ix] = match &self.keys[ix] {
                Some(key) => settled(key),
                None => Some(None),
            };
        }
    }

    /// The shapes to lay the cards out with.
    ///
    /// With `old`, only what the cache learned has changed since that
    /// layout, and the cards above the view, those whose bottom is above
    /// `top` in it, keep the shapes they had while any of them is still a
    /// guess. Each shape that settles above the view moves every card under
    /// it in its column, and with the shapes arriving one at a time from the
    /// sweep, that was a second or more of jitter after a jump down a large
    /// folder. Held, they land in one layout once the last is known.
    ///
    /// Without `old`, a first layout or one for a new width, zoom or set of
    /// card details, every card takes the best shape known.
    fn aspects(
        &mut self,
        old: Option<&Masonry>,
        top: f32,
        changed: Vec<usize>,
    ) -> Vec<Option<f32>> {
        let count = self.keys.len();
        let fresh = |ix: usize| self.settled[ix].unwrap_or(self.guesses[ix]);
        let Some(old) = old.filter(|_| self.aspects.len() == count) else {
            self.held.clear();
            return (0..count).map(fresh).collect();
        };
        let mut aspects = self.aspects.clone();
        let above = |ix: usize| old.rect(ix).is_some_and(|(_, y, _, h)| y + h <= top);
        let unsettled_above =
            (0..count).any(|ix| self.keys[ix].is_some() && self.settled[ix].is_none() && above(ix));
        let mut update = std::mem::take(&mut self.held);
        update.extend(changed);
        update.sort_unstable();
        update.dedup();
        for ix in update {
            if unsettled_above && above(ix) {
                self.held.push(ix);
            } else {
                aspects[ix] = self.settled[ix].unwrap_or(self.guesses[ix]);
            }
        }
        aspects
    }
}

impl DirPane {
    pub(super) fn render_grid(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds = self.grid_scroll.bounds();
        let measured = bounds.size.width > px(0.);
        let (width, height) = if measured {
            (f32::from(bounds.size.width), f32::from(bounds.size.height))
        } else {
            // Before the grid's first frame: the list's last measure when
            // there is one, which is the same space, so switching from the
            // list does not lay out twice.
            self.scroll
                .0
                .borrow()
                .last_item_size
                .map(|size| (f32::from(size.item.width), f32::from(size.item.height)))
                .filter(|(w, _)| *w > 0.)
                .unwrap_or(FIRST_GUESS)
        };
        let scale = window.scale_factor();
        let previews = preview::enabled(cx);
        // The scrollbar is laid over the right edge, so the cards stop short
        // of it rather than sit under it.
        self.grid_layout(width - crate::scrollbar::WIDTH, scale, previews, cx);

        let mut state = self.grid.borrow_mut();
        state.viewport = height;
        state.measured = measured;
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
        // Each card with the shape the layout gave it, which may be one held
        // from before, so the picture always fills the card it is in.
        let keys: Vec<Option<(Key, f32)>> = near
            .iter()
            .map(|&ix| {
                let key = state.keys.get(ix).cloned().flatten()?;
                Some((key, state.aspects.get(ix).copied().flatten()?))
            })
            .collect();
        let sweep = state.sweep.clone();
        drop(state);
        preview::want(owner, wants, cx);
        // Every card's shape, in the background, top first: the ones above
        // the view are what move the cards in it when they change, so they
        // are settled before the view gets there.
        if let Some(sweep) = sweep {
            preview::sweep_shapes(owner, sweep, cx);
        }

        let now = SystemTime::now();
        let cards: Vec<_> = near
            .iter()
            .zip(keys)
            .filter_map(|(&ix, key)| {
                let rect = layout.rect(ix)?;
                Some(self.render_card(ix, rect, key, now, cx))
            })
            .collect();

        // ctrl-scroll zooms. On the content rather than on the scrolling div,
        // because gpui calls the front-most listener first: this one runs
        // before the div's own, and stopping here keeps the wheel from also
        // scrolling. At least the view's height, so the empty space under a
        // short grid zooms too.
        let content = div()
            .relative()
            .w_full()
            .h(px(layout.total_height().max(height)))
            .on_scroll(cx.listener(|this, event: &gpui::ScrollEvent, window, cx| {
                if !event.modifiers.control {
                    return;
                }
                cx.stop_propagation();
                let dy = event.delta.pixel_delta(window.line_height()).y;
                if dy > px(0.) {
                    this.zoom(Some(true), cx);
                } else if dy < px(0.) {
                    this.zoom(Some(false), cx);
                }
            }))
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
            target: self.view.card_width,
            details: self.view.card_details.iter().count(),
            log: preview::log_end(cx),
            scale,
            previews,
        };
        let mut state = self.grid.borrow_mut();
        if state.keys_for != Some(self.listing_moved)
            || state
                .made_from
                .is_some_and(|m| m.scale != scale || m.previews != previews)
        {
            state.reset_keys(self.grid_keys(scale, previews), self.listing_moved);
        }
        if state.made_from == Some(inputs) && state.layout.is_some() {
            return;
        }

        let log = state.seq.and_then(|seq| preview::changes_since(seq, cx));
        let changed = state.changed_cards(log, preview::log_end(cx));
        state.settle(&changed, |key| preview::settled_aspect(key, cx));

        // Only what the cache learned changed, so the cards above the view
        // can be held: see `GridState::aspects`.
        let only_learned = state.made_from.is_some_and(|made| {
            made == (Inputs {
                log: made.log,
                ..inputs
            })
        });
        let old = state.layout.take();
        let aspects = state.aspects(
            old.as_ref().filter(|_| only_learned),
            self.grid_offset(),
            changed,
        );
        let details = inputs.details as f32;
        let target = self.view.card_width as f32;
        let layout = Masonry::new(width, target, self.entries.len(), |ix, card| {
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
        state.aspects = aspects;
    }

    /// Each entry's preview key, or `None` for one with no picture to ask
    /// for: a folder, an archive member, or any entry with previews off.
    fn grid_keys(&self, scale: f32, previews: bool) -> Vec<Option<Key>> {
        self.entries
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
            .collect()
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

    /// Columns of the layout made from the grid's measured width, or 0
    /// before there is one. For the probe: a test reading the count before
    /// the first measured frame would read a guess.
    pub(super) fn grid_columns_measured(&self) -> usize {
        if self.grid.borrow().measured {
            self.grid_columns()
        } else {
            0
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
        picture: Option<(Key, f32)>,
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
            // The lead card is outlined as the lead row is, in the accent
            // colour while this pane has the keys.
            .border_color(if is_lead && self.active {
                // The colour Zed's project panel outlines its active entry
                // in. Some themes make `border_focused` a dark surface tone.
                colors.panel_focused_border
            } else {
                colors.border_variant
            })
            .bg(colors.elevated_surface_background)
            .cursor_pointer()
            .when(selected, |el| el.bg(fill))
            .when(!selected, |el| el.hover(move |style| style.bg(hover)))
            .when_some(picture, |el, (key, aspect)| {
                let width = w - BORDER;
                el.child(preview::card_picture(&key, width, width / aspect, cx))
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

    /// Six image cards in three columns, all at the 4:3 guess for a png.
    fn six_guessed() -> GridState {
        let mut state = GridState::default();
        let keys = (0..6)
            .map(|n| {
                Some(preview::key(
                    preview::Subject {
                        path: Arc::from(Path::new(&format!("/g/{n}.png"))),
                        modified: None,
                        size: Some(n),
                    },
                    1.,
                ))
            })
            .collect();
        state.reset_keys(keys, 1);
        state.aspects = state.aspects(None, 0., Vec::new());
        state
    }

    fn laid_out(state: &GridState) -> Masonry {
        let aspects = state.aspects.clone();
        Masonry::new(332., 100., 6, |ix, card| {
            aspects[ix].map_or(30., |a| card / a)
        })
    }

    fn settle(state: &mut GridState, cards: &[usize], aspect: f32) {
        let keys: Vec<Key> = cards
            .iter()
            .map(|&ix| state.keys[ix].clone().unwrap())
            .collect();
        state.settle(cards, |key| keys.contains(key).then_some(Some(aspect)));
    }

    #[test]
    fn shapes_above_the_view_wait_for_the_last_guess_above_it() {
        let mut state = six_guessed();
        let guess = state.aspects[0];
        // The first band, cards 0 to 2, ends at 8 + 75: a view from 100 has
        // all three above it.
        let old = laid_out(&state);
        let top = 100.;

        // Card 1 settles while 0 and 2 are still guesses: it is held.
        settle(&mut state, &[1], 1.);
        let aspects = state.aspects(Some(&old), top, vec![1]);
        assert_eq!(aspects[1], guess, "held at the shape it had");
        assert_eq!(state.held, [1]);
        state.aspects = aspects;

        // A card in view settles meanwhile: it is not held.
        settle(&mut state, &[4], 0.5);
        let aspects = state.aspects(Some(&old), top, vec![4]);
        assert_eq!(aspects[4], Some(0.5));
        assert_eq!(aspects[1], guess, "and the one above is still held");
        state.aspects = aspects;

        // The last guesses above settle: everything held lands at once.
        settle(&mut state, &[0, 2], 2.);
        let aspects = state.aspects(Some(&old), top, vec![0, 2]);
        assert_eq!(aspects[..3], [Some(2.), Some(1.), Some(2.)]);
        assert!(state.held.is_empty());
    }

    #[test]
    fn nothing_is_held_at_the_top_or_on_a_fresh_layout() {
        let mut state = six_guessed();
        let old = laid_out(&state);
        // At the top nothing is above the view.
        settle(&mut state, &[1], 1.);
        let aspects = state.aspects(Some(&old), 0., vec![1]);
        assert_eq!(aspects[1], Some(1.));
        // Without an old layout, every card takes its best known shape.
        settle(&mut state, &[0], 2.);
        let aspects = state.aspects(None, 500., Vec::new());
        assert_eq!(aspects[0], Some(2.));
        assert!(state.held.is_empty());
    }

    #[test]
    fn a_log_that_no_longer_reaches_back_means_every_card() {
        let mut state = six_guessed();
        assert_eq!(state.changed_cards(None, 42), (0..6).collect::<Vec<_>>());
        assert_eq!(state.seq, Some(42));
        let key = state.keys[3].clone().unwrap();
        assert_eq!(state.changed_cards(Some((vec![key], 43)), 43), [3]);
        assert_eq!(state.seq, Some(43));
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
