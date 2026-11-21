//! Previews in a tooltip: the file under the pointer, without opening it.
//!
//! Five kinds of file get one: an image, a video, an audio file, a PDF and a
//! text file. The picture for the first four comes from the freedesktop
//! thumbnail cache (see `thumbnail`), the tags of an audio file from `lofty`,
//! a PDF's page count from `pdfinfo`, and a text file shows its first lines. Every other file, every folder and every member
//! of an archive shows no tooltip.
//!
//! # Lifecycle
//!
//! gpui calls the row's tooltip builder once, when the pointer has rested for
//! its delay, and draws the view it returns until the pointer leaves the row,
//! the list scrolls, or a button goes down. The view starts empty and fills in
//! when the work arrives. The work is not tied to the view: it finishes and
//! goes into the cache even when the pointer has moved on, so a second look is
//! immediate.
//!
//! # The atlas
//!
//! A picture is handed to gpui as a `RenderImage` rather than as a path. That
//! keeps gpui's own image cache, which never evicts, out of it, and it leaves
//! the picture's texture in each window's atlas until somebody drops it. The
//! loader's cache is that somebody: it drops the texture of each preview it
//! lets go. See `loader`.
//!
//! # Where things are
//!
//! - `facts`: what a file is and its shape, read off the UI thread.
//! - `loader`: the cache and the queue of work, shared by every owner.
//! - `tooltip`: the hover preview of a row.
//! - `thumbnail`, `video`, `pdf`, `audio`, `text`, `syntax`: the readers.
//! - Here: the cards' side (keys, shapes, pictures) and the settings switch.

mod audio;
mod facts;
mod loader;
mod pdf;
mod syntax;
mod text;
mod thumbnail;
mod tooltip;
mod video;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::AnimationExt as _;
use gpui::{
    AnyView, App, Global, IntoElement, Styled as _, StyledImage as _, Window, div, img, px,
};
use theme::ActiveTheme as _;

use self::loader::Known;
use self::thumbnail::Size;
use self::tooltip::PreviewTooltip;
pub use self::tooltip::probe;

/// The card's corner radius (`rounded_md`) inside its one-pixel border. A
/// picture at the top of the card takes these corners.
const INNER_RADIUS: f32 = 5.;

/// The file a row shows, as the row knows it when it is drawn.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Subject {
    pub path: Arc<Path>,
    pub modified: Option<SystemTime>,
    pub size: Option<u64>,
}

/// The `tooltip` builder for a row.
pub fn tooltip(subject: Subject) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| PreviewTooltip::open(subject.clone(), window.scale_factor(), cx).into()
}

// What owners of previews use straight from the loader: see its module
// documentation.
pub use self::loader::{Key, Need, changes_since, forget, log_end, sweep as sweep_shapes, want};

/// The key a card or a tooltip asks for: the file, and the thumbnail size the
/// window's scale calls for.
pub fn key(subject: Subject, scale: f32) -> Key {
    Key {
        subject,
        size: Size::for_scale(scale),
    }
}

/// The narrowest and the widest a card's picture is drawn, as width over
/// height. A long screenshot or a panorama is cropped to this, centered,
/// rather than drawn as a card that fills the screen or a sliver.
const CARD_ASPECTS: (f32, f32) = (0.5, 2.);

/// A card's settled shape, width over height and clamped, or `None` while it
/// is not settled. The inner `None` is a card with no picture.
///
/// Settled from the facts when the file's header could be read, else from
/// the picture once it is made. Before either, a layout uses
/// `guessed_aspect`, so it can place every card at once and correct the few
/// the guess got wrong.
pub fn settled_aspect(key: &Key, cx: &App) -> Option<Option<f32>> {
    Some(loader::shape(key, cx)?.map(clamp_aspect))
}

/// A card's shape from its extension alone, clamped.
pub fn guessed_aspect(key: &Key) -> Option<f32> {
    guess_aspect(&key.subject.path).map(clamp_aspect)
}

fn clamp_aspect(aspect: f32) -> f32 {
    aspect.clamp(CARD_ASPECTS.0, CARD_ASPECTS.1)
}

/// A shape from the extension alone: what most files of that kind are.
fn guess_aspect(path: &Path) -> Option<f32> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif"
        | "avif" | "jxl" | "svg" | "qoi" | "ico" => Some(4. / 3.),
        "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp"
        | "ts" | "ogv" => Some(16. / 9.),
        "pdf" => Some(8.5 / 11.),
        _ => None,
    }
}

/// Whether the picture for `key` is made and can be drawn.
pub fn has_picture(key: &Key, cx: &App) -> bool {
    loader::known(key, cx)
        .done()
        .is_some_and(|preview| preview.picture.is_some())
}

/// A card's picture, `width` by `height`, cropped to fill it; or a box that
/// breathes in its place while it is made.
pub fn card_picture(key: &Key, width: f32, height: f32, cx: &App) -> gpui::AnyElement {
    let hold = cx.theme().colors().element_hover;
    match loader::known(key, cx) {
        Known::Done(preview) => match &preview.picture {
            Some(picture) => img(picture.image.clone())
                .w(px(width))
                .h(px(height))
                .object_fit(gpui::ObjectFit::Cover)
                .rounded_t(px(INNER_RADIUS))
                .into_any_element(),
            None => div().w(px(width)).h(px(height)).into_any_element(),
        },
        _ => placeholder(width, height, hold, |el| el.rounded_t(px(INNER_RADIUS))),
    }
}

/// Remove the working files of thumbnailers that hojas no longer running
/// started. Blocks on the file system: call it off the UI thread.
pub fn sweep() {
    thumbnail::sweep();
}

/// Whether rows get a preview, from the `previews` setting.
pub fn enabled(cx: &App) -> bool {
    cx.try_global::<Enabled>().is_none_or(|on| on.0)
}

/// `None` means the default, which is on.
pub fn set_enabled(on: Option<bool>, cx: &mut App) {
    cx.set_global(Enabled(on.unwrap_or(true)));
}

struct Enabled(bool);
impl Global for Enabled {}

/// A box that breathes where the picture will be, at the picture's size.
/// The same pulse the Size column uses while it counts.
fn placeholder(
    width: f32,
    height: f32,
    color: gpui::Hsla,
    corners: impl FnOnce(gpui::Div) -> gpui::Div,
) -> gpui::AnyElement {
    corners(div())
        .w(px(width))
        .h(px(height))
        .bg(color)
        .with_animation(
            "preview-placeholder",
            gpui::Animation::new(Duration::from_millis(1400))
                .repeat()
                .with_easing(gpui::pulsating_between(0.35, 0.8)),
            |el, delta| el.opacity(delta),
        )
        .into_any_element()
}
