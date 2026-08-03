//! A vertical scrollbar for a `uniform_list`.
//!
//! gpui has no scrollbar. `list.rs` reserves the internals one would need and
//! stops there, so the widget belongs a layer up; Zed's answer lives in its `ui`
//! crate, which would bring `component`, `icons`, `menu`, `schemars`, `strum`
//! and half a dozen more into a tree that currently takes four crates from Zed.
//! That is a lot of surface pinned to one revision for one widget, and the
//! whole of what a scrollbar needs from a scroll handle is four numbers:
//!
//! ```text
//! offset      how far down we are, negative and growing downward
//! max_offset  how far down we can go
//! viewport    how tall the window onto the content is
//! set_offset  put us somewhere else
//! ```
//!
//! Two of those four never arrive, which is why this is an `Element` and not a
//! `div` assembled in `render`.
//!
//! `ScrollHandle`'s `bounds` and `max_offset` are written in
//! `Interactivity::prepaint`, in the branch an element takes when it scrolls
//! *through* the interactivity layer. A `uniform_list` scrolls itself and never
//! reaches it: `bounds()` stays zero-sized and `max_offset()` comes back as the
//! whole content height, having subtracted a viewport of nothing. Reading them
//! from `render` compounds it, because `render` runs before layout.
//!
//! So the track comes from this element's *own* bounds in `prepaint`, which is
//! after the list beside it has been laid out, and the content height comes
//! from the caller, which knows its row count and row height. Only `offset` is
//! taken from the handle, and that one uniform_list does maintain. This is what
//! Zed's `ScrollbarPrepaintState` is for.
//!
//! # Two things that cost a long time
//!
//! `Length::default()` is `Definite(0px)` and not `Auto`, so filling the rest
//! of an `Edges` from `Default` pins `left` to zero alongside `right`. Both at
//! zero resolves to left, and the bar drew itself down the far side of the
//! pane, outside every screenshot taken of the right edge. Start from
//! `Edges::auto()`.
//!
//! A hitbox inserted as `HitboxBehavior::Normal` does not stop the press
//! reaching what is behind it, so clicking the bar grabbed the file underneath
//! and started dragging it, and this element's own handlers never ran.
//! `BlockMouseExceptScroll` fixes both: it blocks the row, and it still lets
//! the wheel through with the cursor over the bar.

use std::panic::Location;

use gpui::{
    App, Bounds, Element, ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Style, UniformListScrollHandle, Window, fill, point, px, size,
};
use theme::ActiveTheme;

/// Wide enough to hit without aiming, narrow enough not to be furniture.
pub const WIDTH: f32 = 10.;
/// A thumb shorter than this is a thumb nobody can grab, however long the
/// listing is. At a hundred thousand rows the honest height is under a pixel.
const MIN_THUMB: f32 = 24.;

/// Where the thumb sits inside a track of `track_height`, or `None` when there
/// is nothing to scroll.
///
/// Split out from the element because it is the whole of the logic and the only
/// part that can be wrong in a way tests can catch.
pub fn thumb(offset: f32, max_offset: f32, track_height: f32) -> Option<(f32, f32)> {
    if max_offset <= 0. || track_height <= 0. {
        return None;
    }
    // The content is the viewport plus everything above and below it.
    let content = track_height + max_offset;
    let height = (track_height * (track_height / content)).max(MIN_THUMB);
    // How far through the scroll we are, nought to one.
    let progress = (offset / max_offset).clamp(0., 1.);
    // The thumb runs out of room by its own height, which is why this is not
    // simply `progress * track_height`: at the bottom the two edges must meet.
    let top = progress * (track_height - height);
    Some((top, height))
}

/// Turn a drag on the track into an offset.
///
/// `grab` is where inside the thumb the press landed, so the thumb does not
/// jump under the cursor on the first pixel of movement.
pub fn offset_for(y: f32, grab: f32, max_offset: f32, track_height: f32) -> f32 {
    let Some((_, height)) = thumb(0., max_offset, track_height) else {
        return 0.;
    };
    let travel = track_height - height;
    if travel <= 0. {
        return 0.;
    }
    (((y - grab) / travel).clamp(0., 1.)) * max_offset
}

/// Held by whatever owns the list, so a drag survives between frames.
#[derive(Default)]
pub struct ScrollbarState {
    /// Where in the thumb the press landed, while a drag is in progress.
    ///
    /// The only thing worth remembering between frames. Hover comes from the
    /// hitbox, which knows it without being told.
    grab: Option<f32>,
}

/// A scrollbar over a `uniform_list`.
///
/// Laid over the list rather than beside it: one that took width would reflow
/// every row the moment a directory grew past one screen.
pub struct Scrollbar<T: 'static> {
    handle: UniformListScrollHandle,
    rows: usize,
    /// Needed to turn a position on the track back into a row, which is the
    /// only thing a `uniform_list` will actually scroll to.
    row_height: f32,
    owner: Entity<T>,
    state: fn(&mut T) -> &mut ScrollbarState,
}

pub fn scrollbar<T: 'static>(
    handle: UniformListScrollHandle,
    rows: usize,
    row_height: f32,
    owner: Entity<T>,
    state: fn(&mut T) -> &mut ScrollbarState,
) -> Scrollbar<T> {
    Scrollbar {
        handle,
        rows,
        row_height,
        owner,
        state,
    }
}

/// What `prepaint` worked out and `paint` needs.
pub struct Prepainted {
    hitbox: Hitbox,
    /// `None` when the content fits and there is nothing to drag.
    thumb: Option<(f32, f32)>,
    max_offset: f32,
    track_height: f32,
}

impl<T: 'static> Scrollbar<T> {
    /// How far down we are. gpui's offsets grow downward as negatives, so this
    /// is where the sign goes; everything else here works in positives.
    fn offset(&self) -> f32 {
        (-f32::from(self.handle.0.borrow().base_handle.offset().y)).max(0.)
    }
}

/// Put the list somewhere.
///
/// By row, because a `uniform_list` will not be moved any other way. Writing
/// `base_handle.set_offset` takes: read it back and the new value is there.
/// The list simply does not consult it when deciding what to draw, so the rows
/// stay exactly where they were. `scroll_to_item` sets `deferred_scroll_to_item`,
/// which its prepaint does consult, and which is what `reveal` already uses.
///
/// The cost is that a drag lands on row boundaries instead of pixels. In a
/// listing of fixed-height rows that is invisible.
fn scroll_to(handle: &UniformListScrollHandle, offset: f32, row_height: f32, rows: usize) {
    if rows == 0 {
        return;
    }
    let ix = ((offset / row_height).round() as usize).min(rows - 1);
    handle.scroll_to_item(ix, gpui::ScrollStrategy::Top);
}

impl<T: 'static> IntoElement for Scrollbar<T> {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl<T: 'static> Element for Scrollbar<T> {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("scrollbar".into()))
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // Absolute and pinned to the right edge over the full height of the
        // parent, so the track is the list's own viewport without measuring it.
        // From `Edges::auto()`, never `Edges::default()`. `Length::default()`
        // is `Definite(0px)` rather than `Auto`, so filling the rest of the
        // struct from `Default` pinned `left` to zero as well; left and right
        // both at zero resolves to left, and the bar drew itself down the far
        // side of the pane. Every screenshot I took was of the right edge.
        let style = Style {
            position: gpui::Position::Absolute,
            inset: gpui::Edges {
                top: px(0.).into(),
                right: px(0.).into(),
                ..gpui::Edges::auto()
            },
            size: gpui::Size {
                width: px(WIDTH).into(),
                height: gpui::relative(1.).into(),
            },
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Prepainted {
        // Our own bounds, and by now the list beside us has been laid out, so
        // this is the height a reader actually sees.
        let track_height = f32::from(bounds.size.height);
        let max_offset = (self.rows as f32 * self.row_height - track_height).max(0.);
        Prepainted {
            // Blocks what is underneath, or a press on the bar reaches the row
            // behind it and starts dragging a file. Except scroll, so the wheel
            // still works with the cursor over the bar, which is what every
            // other scrollbar does.
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll),
            thumb: thumb(self.offset(), max_offset, track_height),
            max_offset,
            track_height,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((top, height)) = prepaint.thumb else {
            return;
        };
        let owner = self.owner.clone();
        let state = self.state;
        let hovered = prepaint.hitbox.is_hovered(window);
        // Through `update` because the accessor hands out `&mut`; nothing is
        // changed here, so no notify follows.
        let dragging = owner.update(cx, |this, _| state(this).grab.is_some());

        let colors = cx.theme().colors();
        let fill_color: Hsla = if dragging {
            colors.scrollbar_thumb_active_background
        } else if hovered {
            colors.scrollbar_thumb_hover_background
        } else {
            colors.scrollbar_thumb_background
        };

        // Inside a content mask, which is how Zed's own scrollbar does it:
        // handlers registered outside one never see an event.
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            let thumb_bounds = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + px(top)),
                size(bounds.size.width, px(height)),
            );
            // Square. A rounded thumb reads as a pill floating over the listing;
            // square, it reads as the edge of the pane, which is what it is.
            window.paint_quad(fill(thumb_bounds, fill_color));

            let track_top = f32::from(bounds.origin.y);
            let (row_height, rows) = (self.row_height, self.rows);
            let (max_offset, track_height) = (prepaint.max_offset, prepaint.track_height);
            let handle = self.handle.clone();
            let hitbox = prepaint.hitbox.clone();

            // Registered here rather than on a `div`, because a raw element has no
            // interactivity layer to hang listeners on.
            window.on_mouse_event({
                let owner = owner.clone();
                let handle = handle.clone();
                move |event: &MouseDownEvent, phase, window, cx| {
                    if !phase.bubble() || !hitbox.is_hovered(window) {
                        return;
                    }
                    let y = f32::from(event.position.y) - track_top;
                    let grab = if y >= top && y < top + height {
                        // On the thumb: remember where, so it does not jump under
                        // the cursor on the first pixel of movement.
                        y - top
                    } else {
                        // On the track: centre the thumb here and carry on as
                        // though it had been grabbed in the middle.
                        let grab = height / 2.;
                        scroll_to(
                            &handle,
                            offset_for(y, grab, max_offset, track_height),
                            row_height,
                            rows,
                        );
                        grab
                    };
                    owner.update(cx, |this, cx| {
                        state(this).grab = Some(grab);
                        cx.notify();
                    });
                }
            });

            window.on_mouse_event({
                let owner = owner.clone();
                let handle = handle.clone();
                move |event: &MouseMoveEvent, phase, _window, cx| {
                    if !phase.bubble() {
                        return;
                    }
                    let Some(grab) = owner.update(cx, |this, _| state(this).grab) else {
                        return;
                    };
                    let y = f32::from(event.position.y) - track_top;
                    scroll_to(
                        &handle,
                        offset_for(y, grab, max_offset, track_height),
                        row_height,
                        rows,
                    );
                    owner.update(cx, |_, cx| cx.notify());
                }
            });

            // Anywhere, because a drag that ends off the bar is the common one: the
            // cursor leaves sideways long before the button comes up.
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                if !phase.bubble() {
                    return;
                }
                owner.update(cx, |this, cx| {
                    if state(this).grab.take().is_some() {
                        cx.notify();
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_scroll_means_no_thumb() {
        assert_eq!(thumb(0., 0., 400.), None);
        assert_eq!(thumb(0., 100., 0.), None);
    }

    #[test]
    fn the_thumb_is_the_visible_fraction() {
        // Viewport 400 of 800 content: half the track, at the top.
        let (top, height) = thumb(0., 400., 400.).unwrap();
        assert_eq!(height, 200.);
        assert_eq!(top, 0.);
    }

    #[test]
    fn the_bottom_of_the_scroll_is_the_bottom_of_the_track() {
        // The thumb runs out of room by its own height. Scaling `top` by the
        // full track instead would push it past the end at the last row.
        let (top, height) = thumb(400., 400., 400.).unwrap();
        assert_eq!(top + height, 400.);
    }

    #[test]
    fn a_thumb_stays_grabbable_however_long_the_listing() {
        // A hundred thousand rows of 22px in a 600px viewport: the honest
        // height is a third of a pixel.
        let max = 100_000. * 22. - 600.;
        let (top, height) = thumb(max, max, 600.).unwrap();
        assert_eq!(height, MIN_THUMB);
        assert!(
            (top + height - 600.).abs() < 0.01,
            "still ends at the bottom"
        );
    }

    #[test]
    fn dragging_maps_the_track_onto_the_offset() {
        // Grabbed at the thumb's top edge, so the cursor's y is the thumb's.
        assert_eq!(offset_for(0., 0., 400., 400.), 0.);
        assert_eq!(offset_for(200., 0., 400., 400.), 400.);
        // Past either end clamps rather than overscrolling.
        assert_eq!(offset_for(-50., 0., 400., 400.), 0.);
        assert_eq!(offset_for(9999., 0., 400., 400.), 400.);
    }

    #[test]
    fn the_grab_point_keeps_the_thumb_under_the_cursor() {
        // Pressing halfway down the thumb and moving to the same y must not
        // move the content at all.
        let (top, height) = thumb(100., 400., 400.).unwrap();
        let grab = height / 2.;
        let at = top + grab;
        assert!((offset_for(at, grab, 400., 400.) - 100.).abs() < 0.01);
    }
}
