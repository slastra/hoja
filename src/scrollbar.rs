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
//! All four are public on `UniformListScrollHandle`, so this is arithmetic.
//!
//! # Not finished
//!
//! The arithmetic below is right and tested. The element is not wired up,
//! because two of those four numbers never arrive.
//!
//! `ScrollHandle`'s `bounds` and `max_offset` are written in `Interactivity::
//! prepaint`, in the branch taken by an element that scrolls *through* the
//! interactivity layer. A `uniform_list` scrolls itself, so it never reaches
//! that branch: `bounds()` stays zero-sized and `max_offset()` comes back as
//! the whole content height, having subtracted a viewport of nothing. Reading
//! them from `render` compounds it, since `render` runs before layout in any
//! case.
//!
//! The fix is to be a real `Element` and read the track from *its own* bounds
//! in `prepaint`, which is after the list beside it has been laid out, and to
//! take the content height from the caller, which knows its row count and row
//! height. That is how Zed's `ui::Scrollbars` does it, and why it carries a
//! `ScrollbarPrepaintState`.

use gpui::{
    Bounds, Context, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, UniformListScrollHandle, div, prelude::*, px,
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
    grab: Option<f32>,
    hovered: bool,
}

/// Read the four numbers out of the handle.
///
/// gpui's offsets grow *downward as negatives*, which is the one thing here
/// worth stating: scrolled to the top is zero and scrolled to the bottom is
/// `-max`. Everything below works in positives, so this is where the sign goes.
fn geometry(handle: &UniformListScrollHandle) -> (f32, f32, Bounds<Pixels>) {
    let state = handle.0.borrow();
    let offset = -f32::from(state.base_handle.offset().y);
    let max = f32::from(state.base_handle.max_offset().y);
    (offset.max(0.), max.max(0.), state.base_handle.bounds())
}

/// The bar itself, to be laid over the right edge of the list.
///
/// Rendered by the owner rather than wrapping the list, because a
/// `uniform_list` measures itself and putting it inside another scroller would
/// give it a viewport that is not the one the user sees.
///
/// `bar` is read now and `state` reaches the same field from inside the
/// listeners, which fire later against `&mut T`. One argument cannot do both.
pub fn render<T: 'static>(
    handle: UniformListScrollHandle,
    bar: &ScrollbarState,
    state: impl Fn(&mut T) -> &mut ScrollbarState + Copy + 'static,
    cx: &mut Context<T>,
) -> Option<gpui::AnyElement> {
    let (offset, max, bounds) = geometry(&handle);
    let track_height = f32::from(bounds.size.height);
    let (top, height) = thumb(offset, max, track_height)?;

    let colors = cx.theme().colors();
    let fill: Hsla = if bar.grab.is_some() {
        colors.scrollbar_thumb_active_background
    } else if bar.hovered {
        colors.scrollbar_thumb_hover_background
    } else {
        colors.scrollbar_thumb_background
    };

    let track_top = bounds.origin.y;
    let for_down = handle.clone();
    let for_move = handle;

    Some(
        div()
            .id("scrollbar")
            .occlude()
            .absolute()
            .top(track_top)
            .right_0()
            .w(px(WIDTH))
            .h(px(track_height))
            .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                state(this).hovered = *over;
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    let y = f32::from(event.position.y - track_top);
                    let grab = if y >= top && y < top + height {
                        // On the thumb: remember where, so it does not jump
                        // under the cursor on the first pixel of movement.
                        y - top
                    } else {
                        // On the track: centre the thumb here and carry on as
                        // though it had been grabbed in the middle, which is
                        // what every other scrollbar does.
                        let grab = height / 2.;
                        let (_, max, bounds) = geometry(&for_down);
                        let to = offset_for(y, grab, max, f32::from(bounds.size.height));
                        set_offset(&for_down, to);
                        grab
                    };
                    state(this).grab = Some(grab);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let Some(grab) = state(this).grab else { return };
                let (_, max, bounds) = geometry(&for_move);
                let y = f32::from(event.position.y - track_top);
                set_offset(
                    &for_move,
                    offset_for(y, grab, max, f32::from(bounds.size.height)),
                );
                cx.notify();
            }))
            // Both, because a drag that ends off the bar is the common one:
            // the cursor leaves sideways long before the button comes up.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    state(this).grab = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    state(this).grab = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(top))
                    .w(px(WIDTH))
                    .h(px(height))
                    .rounded_full()
                    .bg(fill),
            )
            .into_any_element(),
    )
}

fn set_offset(handle: &UniformListScrollHandle, offset: f32) {
    let state = handle.0.borrow();
    let mut point: Point<Pixels> = state.base_handle.offset();
    point.y = px(-offset);
    state.base_handle.set_offset(point);
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
