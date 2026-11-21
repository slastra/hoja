//! Where each card goes in the grid layout. Pure arithmetic, no gpui.
//!
//! Entries are dealt into columns, not packed: entry *i* goes to column
//! *i* mod *N*. The sort order then reads left to right across each band of
//! *N*, whatever the heights, and the keyboard is plain index arithmetic:
//! left and right are ±1, up and down are ±*N*. The price is that the columns
//! end at different heights. Packing each card into the shortest column ends
//! them level, but it scatters the order sideways.
//!
//! Each column is a stack of known heights, so its tops increase down the
//! column, and the cards in view are a binary search per column.

/// Space between cards, and between the cards and the edges.
pub const GAP: f32 = 8.;

/// The cards' positions for one width and one set of heights.
#[derive(Debug, Clone, PartialEq)]
pub struct Masonry {
    columns: usize,
    card_width: f32,
    tops: Vec<f32>,
    heights: Vec<f32>,
    total_height: f32,
}

/// Columns for `width`, aiming at cards about `target` wide.
pub fn columns_for(width: f32, target: f32) -> usize {
    (((width - GAP) / (target + GAP)).floor() as usize).max(1)
}

impl Masonry {
    /// Lay out `count` cards across `width`. `height` gives a card's height
    /// from its index and the card width, which the layout decides.
    pub fn new(width: f32, target: f32, count: usize, height: impl Fn(usize, f32) -> f32) -> Self {
        let columns = columns_for(width, target);
        let card_width = ((width - GAP * (columns as f32 + 1.)) / columns as f32).max(1.);
        let mut bottoms = vec![GAP; columns];
        let mut tops = Vec::with_capacity(count);
        let mut heights = Vec::with_capacity(count);
        for ix in 0..count {
            let h = height(ix, card_width).max(1.);
            let column = ix % columns;
            tops.push(bottoms[column]);
            heights.push(h);
            bottoms[column] += h + GAP;
        }
        let total_height = bottoms.into_iter().fold(GAP, f32::max);
        Masonry {
            columns,
            card_width,
            tops,
            heights,
            total_height,
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn len(&self) -> usize {
        self.tops.len()
    }

    pub fn total_height(&self) -> f32 {
        self.total_height
    }

    /// Card `ix` as (x, y, width, height), from the top left of the content.
    pub fn rect(&self, ix: usize) -> Option<(f32, f32, f32, f32)> {
        let column = ix % self.columns;
        let x = GAP + column as f32 * (self.card_width + GAP);
        Some((x, *self.tops.get(ix)?, self.card_width, self.heights[ix]))
    }

    pub fn top(&self, ix: usize) -> Option<f32> {
        self.tops.get(ix).copied()
    }

    /// The cards that reach into `top..bottom`, in index order.
    pub fn visible(&self, top: f32, bottom: f32) -> Vec<usize> {
        let mut found = Vec::new();
        for column in 0..self.columns.min(self.len()) {
            // This column's cards are `column`, `column + N`, ..., and their
            // tops increase, so the first one in view is a binary search on
            // where its bottom passes `top`.
            let count = (self.len() - column).div_ceil(self.columns);
            let at = |k: usize| column + k * self.columns;
            let first = partition_point(count, |k| {
                let ix = at(k);
                self.tops[ix] + self.heights[ix] <= top
            });
            for k in first..count {
                let ix = at(k);
                if self.tops[ix] >= bottom {
                    break;
                }
                found.push(ix);
            }
        }
        found.sort_unstable();
        found
    }

    /// The first card in index order whose top edge is in view, or failing
    /// that the first card in view at all. What Back puts at the top again.
    pub fn first_in_view(&self, top: f32, bottom: f32) -> Option<usize> {
        let visible = self.visible(top, bottom);
        visible
            .iter()
            .copied()
            .find(|&ix| self.tops[ix] >= top)
            .or_else(|| visible.first().copied())
    }

    /// The card a page key lands on from `ix`: the one in the same column
    /// whose top is nearest a page of `page` pixels away, and at least one
    /// card away when there is one.
    pub fn page_from(&self, ix: usize, page: f32, down: bool) -> Option<usize> {
        let top = self.top(ix)?;
        let target = if down { top + page } else { top - page };
        let column = ix % self.columns;
        let count = (self.len() - column).div_ceil(self.columns);
        let k = ix / self.columns;
        let at = |k: usize| column + k * self.columns;
        let best = if down {
            // The first card in the column at or below the target.
            let below = partition_point(count, |k| self.tops[at(k)] < target);
            below.max(k + 1).min(count - 1)
        } else {
            // The last card in the column at or above the target.
            let above = partition_point(count, |k| self.tops[at(k)] <= target);
            above.saturating_sub(1).min(k.saturating_sub(1))
        };
        Some(at(best))
    }
}

/// The first `k` in `0..count` for which `before(k)` is false, given that it
/// is true up to some point and false after.
fn partition_point(count: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three columns of 100 across 332: 8 + 3 * (100 + 8).
    const WIDTH: f32 = 332.;

    fn heights(list: &'static [f32]) -> impl Fn(usize, f32) -> f32 {
        move |ix, _| list[ix]
    }

    #[test]
    fn the_column_count_follows_the_width() {
        assert_eq!(columns_for(WIDTH, 100.), 3);
        assert_eq!(columns_for(WIDTH - 1., 100.), 2);
        assert_eq!(columns_for(10., 100.), 1, "never none");
    }

    #[test]
    fn cards_are_dealt_in_order_and_stack_in_their_column() {
        let layout = Masonry::new(WIDTH, 100., 7, heights(&[50., 80., 30., 40., 10., 20., 5.]));
        assert_eq!(layout.columns(), 3);
        // The first band: left to right, all at the top.
        assert_eq!(layout.rect(0), Some((8., 8., 100., 50.)));
        assert_eq!(layout.rect(1), Some((116., 8., 100., 80.)));
        assert_eq!(layout.rect(2), Some((224., 8., 100., 30.)));
        // Entry 3 is under entry 0, whatever the other columns did.
        assert_eq!(layout.top(3), Some(8. + 50. + 8.));
        assert_eq!(layout.top(4), Some(8. + 80. + 8.));
        assert_eq!(layout.top(6), Some(66. + 40. + 8.));
        // The tallest column, with the gap under it.
        assert_eq!(layout.total_height(), 114. + 5. + 8.);
    }

    #[test]
    fn the_cards_in_view_are_found_in_every_column() {
        // Ten cards of 100 in each of three columns.
        let layout = Masonry::new(WIDTH, 100., 30, |_, _| 100.);
        // Bands start at 8, 116, 224, 332...: 150..250 cuts bands 1 and 2.
        assert_eq!(layout.visible(150., 250.), [3, 4, 5, 6, 7, 8]);
        assert_eq!(layout.visible(0., 10.), [0, 1, 2]);
        assert!(layout.visible(5000., 6000.).is_empty());
        // Band 2 is the first whose top is in view.
        assert_eq!(layout.first_in_view(150., 250.), Some(6));
        assert_eq!(
            layout.first_in_view(120., 130.),
            Some(3),
            "only cut through"
        );
    }

    #[test]
    fn uneven_columns_are_searched_on_their_own() {
        // One tall card in column 0 pushes its column down, not the others.
        let layout = Masonry::new(WIDTH, 100., 6, heights(&[500., 10., 10., 10., 10., 10.]));
        assert_eq!(layout.visible(0., 100.), [0, 1, 2, 4, 5]);
        assert_eq!(layout.visible(509., 600.), [3]);
    }

    #[test]
    fn a_page_stays_in_its_column() {
        let layout = Masonry::new(WIDTH, 100., 30, |_, _| 100.);
        // From card 1 (column 1, top 8), a 300px page down lands on the card
        // whose top is at or past 308: band 3, card 10.
        assert_eq!(layout.page_from(1, 300., true), Some(10));
        assert_eq!(layout.page_from(10, 300., false), Some(1));
        // Clamped to the column's ends.
        assert_eq!(layout.page_from(28, 300., true), Some(28));
        assert_eq!(layout.page_from(1, 300., false), Some(1));
        // A page shorter than a card still moves one card.
        assert_eq!(layout.page_from(1, 10., true), Some(4));
        assert_eq!(layout.page_from(4, 10., false), Some(1));
    }

    #[test]
    fn nothing_to_lay_out() {
        let layout = Masonry::new(WIDTH, 100., 0, |_, _| 100.);
        assert_eq!(layout.len(), 0);
        assert!(layout.visible(0., 1000.).is_empty());
        assert_eq!(layout.first_in_view(0., 1000.), None);
        assert_eq!(layout.total_height(), GAP);
    }
}
