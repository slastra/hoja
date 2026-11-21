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
//! cache here is that somebody: it holds the last `KEEP` previews and drops
//! the texture of the one it lets go.

mod audio;
mod loader;
mod pdf;
mod text;
mod thumbnail;
mod video;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::AnimationExt as _;
use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EntityId, Global, IntoElement,
    ParentElement as _, Render, RenderImage, SharedString, Styled as _, StyledImage as _, Window,
    div, img, prelude::FluentBuilder as _, px,
};
use theme::ActiveTheme as _;

use self::loader::Known;
use self::thumbnail::Size;

/// The width of an image's or a video's picture, in logical pixels.
const PICTURE: f32 = 256.;
/// The tallest a picture is drawn. A taller one is drawn narrower, so a
/// portrait phone video does not run off a laptop screen.
const TALLEST: f32 = 360.;
/// The long side of an audio file's picture, beside its tags.
const COVER: f32 = 96.;
/// The widest a tooltip grows, for a text file's long lines.
const MAX_WIDTH: f32 = 640.;
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

pub use self::loader::{Key, Need};

/// The key a card or a tooltip asks for: the file, and the thumbnail size the
/// window's scale calls for.
pub fn key(subject: Subject, scale: f32) -> Key {
    Key {
        subject,
        size: Size::for_scale(scale),
    }
}

/// Replace everything `owner` wants. See `loader`.
pub fn want(owner: EntityId, wants: Vec<(Key, Need, u32)>, cx: &mut App) {
    loader::want(owner, wants, cx);
}

/// `owner` wants nothing any more.
pub fn forget(owner: EntityId, cx: &mut App) {
    loader::forget(owner, cx);
}

/// A count that changes whenever the cache learns anything: a layout made
/// when it was different may have the wrong shapes.
pub fn learned(cx: &App) -> u64 {
    loader::learned_count(cx)
}

/// The narrowest and the widest a card's picture is drawn, as width over
/// height. A long screenshot or a panorama is cropped to this, centered,
/// rather than drawn as a card that fills the screen or a sliver.
const CARD_ASPECTS: (f32, f32) = (0.5, 2.);

/// The shape a card keeps for this file's picture, width over height, or
/// `None` for a card with no picture.
///
/// From the picture when it is made, from the facts before that, and before
/// those from the file's extension, so a layout can place every card at once
/// and correct the few the guess got wrong.
pub fn card_aspect(key: &Key, cx: &App) -> Option<f32> {
    let aspect = match loader::known(key, cx) {
        Known::Done(preview) => preview
            .picture
            .as_ref()
            .map(|p| p.width as f32 / p.height.max(1) as f32),
        Known::Facts(facts) => facts.aspect,
        Known::Nothing => guess_aspect(&key.subject.path),
    }?;
    Some(aspect.clamp(CARD_ASPECTS.0, CARD_ASPECTS.1))
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

/// The open preview, for the probe: its kind and the file's name, with
/// `loading` in front until the picture is done.
pub fn probe(cx: &App) -> Option<String> {
    let (_, key, name) = cx.try_global::<OpenTooltip>()?.0.as_ref()?;
    Some(label(&loader::known(key, cx), name))
}

struct Enabled(bool);
impl Global for Enabled {}

// ---------------------------------------------------------------------------
// What is shown

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Video,
    Audio,
    /// A PDF. Its picture is the first page.
    Document,
    Text,
    /// Nothing to show. The tooltip draws nothing at all.
    Other,
}

impl Kind {
    fn of(mimes: &[String]) -> Kind {
        let Some(mime) = mimes.first() else {
            return Kind::Other;
        };
        if mime.starts_with("image/") {
            Kind::Image
        } else if mime.starts_with("video/") {
            Kind::Video
        } else if mime.starts_with("audio/") {
            Kind::Audio
        } else if mimes.iter().any(|m| m == "application/pdf") {
            Kind::Document
        } else if mimes.iter().any(|m| m == "text/plain") {
            Kind::Text
        } else {
            Kind::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Document => "document",
            Kind::Text => "text",
            Kind::Other => "none",
        }
    }
}

struct Picture {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

/// What is quick to learn about a file: everything but its picture. Reading
/// it costs a header or the first 8 KiB, so it arrives well before a
/// thumbnailer has finished, and the tooltip shows it while it waits.
struct Facts {
    kind: Kind,
    mimes: Vec<String>,
    /// Of the original as a viewer shows it, in pixels.
    dimensions: Option<(u32, u32)>,
    /// Width over height of the picture to come, so its place can be kept
    /// at the right shape. `None` for a file that will have no picture.
    aspect: Option<f32>,
    /// Of a video. An audio file's is in its tags.
    duration: Option<Duration>,
    /// Of a PDF, when `pdfinfo` could say.
    pages: Option<u32>,
    lines: Option<Vec<String>>,
    tags: Option<audio::Tags>,
}

/// The facts and the picture, once the picture is done or has failed.
struct Preview {
    facts: Arc<Facts>,
    picture: Option<Picture>,
    /// The original's size as the thumbnail records it. For a format whose
    /// header `image` cannot read, this is the only place it is known.
    original: Option<(u32, u32)>,
}

/// The quick half, off the UI thread.
fn facts(path: &Path) -> Facts {
    let mimes = crate::opener::mime_chain(path);
    let kind = Kind::of(&mimes);
    let mut facts = Facts {
        kind,
        mimes,
        dimensions: None,
        aspect: None,
        duration: None,
        pages: None,
        lines: None,
        tags: None,
    };
    match kind {
        Kind::Image => {
            facts.dimensions = image_size(path);
            // A format `image` cannot read still gets a picture from its
            // thumbnailer. Its shape is a guess until then.
            facts.aspect = Some(
                facts
                    .dimensions
                    .map_or(4. / 3., |(w, h)| w as f32 / h.max(1) as f32),
            );
        }
        Kind::Video => {
            let video = video::read(path);
            facts.dimensions = video.map(|v| (v.width, v.height));
            facts.duration = video.and_then(|v| v.duration);
            facts.aspect = Some(video.map_or(16. / 9., |v| v.aspect));
        }
        Kind::Audio => {
            facts.tags = audio::read(path);
            // Room for a cover only when the file has one to show.
            facts.aspect = facts
                .tags
                .as_ref()
                .is_some_and(|tags| tags.has_cover)
                .then_some(1.);
        }
        Kind::Document => {
            let pdf = pdf::read(path);
            facts.pages = pdf.map(|pdf| pdf.pages);
            // A portrait Letter page when the first page cannot be measured.
            facts.aspect = Some(pdf.and_then(|pdf| pdf.aspect).unwrap_or(8.5 / 11.));
        }
        Kind::Text => facts.lines = text::excerpt(path),
        Kind::Other => {}
    }
    facts
}

/// The size from the header alone, turned as the photo's EXIF says, which is
/// how the thumbnailer will draw it.
fn image_size(path: &Path) -> Option<(u32, u32)> {
    use image::ImageDecoder as _;
    use image::metadata::Orientation;
    let mut decoder = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (width, height) = decoder.dimensions();
    let turned = matches!(
        decoder.orientation(),
        Ok(Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH)
    );
    Some(if turned {
        (height, width)
    } else {
        (width, height)
    })
}

/// The slow half: the picture, which may run a thumbnailer.
fn picture(path: &Path, facts: Arc<Facts>, size: Size) -> Preview {
    let made = facts
        .aspect
        .and_then(|_| thumbnail::picture(path, &facts.mimes, size));
    let original = made.as_ref().and_then(|made| made.original);
    let picture = made.map(|made| {
        let (width, height) = made.bgra.dimensions();
        Picture {
            image: Arc::new(RenderImage::new([image::Frame::new(made.bgra)])),
            width,
            height,
        }
    });
    Preview {
        facts,
        picture,
        original,
    }
}

// ---------------------------------------------------------------------------
// The view

/// The tooltip on screen, for the probe: its entity, its file, and the
/// file's name.
#[derive(Default)]
struct OpenTooltip(Option<(EntityId, Key, SharedString)>);

impl Global for OpenTooltip {}

/// What the probe says: the kind and the name, with `loading` in front
/// until the work is done.
fn label(known: &Known, name: &str) -> String {
    match known {
        Known::Done(preview) => format!("{}: {name}", preview.facts.kind.name()),
        Known::Facts(facts) => format!("loading {}: {name}", facts.kind.name()),
        Known::Nothing => format!("loading: {name}"),
    }
}

struct PreviewTooltip {
    key: Key,
    size: Option<u64>,
    /// The window's scale when the tooltip opened, which sets the size of the
    /// picture asked for and the size it is drawn at.
    scale: f32,
}

impl PreviewTooltip {
    fn open(subject: Subject, scale: f32, cx: &mut App) -> Entity<Self> {
        let key = Key {
            subject: subject.clone(),
            size: Size::for_scale(scale),
        };
        let name: SharedString = subject
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let tooltip = cx.new(|_| PreviewTooltip {
            key: key.clone(),
            size: subject.size,
            scale,
        });
        let id = tooltip.entity_id();
        // Ahead of everything a grid wants: this is the one file being looked
        // at on purpose.
        loader::want(id, vec![(key.clone(), Need::Picture, 0)], cx);
        cx.set_global(OpenTooltip(Some((id, key, name))));
        // When the tooltip goes, so does what it wanted, and the probe stops
        // naming it unless a newer tooltip has taken its place.
        //
        // gpui releases the view after the frame that stopped drawing it, and
        // the probe for that frame is already written by then, still naming
        // this tooltip. Nothing else asks for a frame, so ask for one, only
        // when a test is reading.
        cx.observe_release(&tooltip, move |_, cx| {
            loader::forget(id, cx);
            let open = cx.default_global::<OpenTooltip>();
            if open.0.as_ref().is_some_and(|(open, _, _)| *open == id) {
                open.0 = None;
                if crate::probe::path().is_some() {
                    cx.refresh_windows();
                }
            }
        })
        .detach();
        tooltip
    }

    /// The line under the body: dimensions, length and rates, then the size.
    fn facts_line(&self, facts: &Facts, preview: Option<&Preview>) -> String {
        let mut line = Vec::new();
        let original = preview.and_then(|p| p.original);
        if let Some((w, h)) = facts.dimensions.or(original) {
            line.push(format!("{w} × {h}"));
        }
        if let Some(duration) = facts.duration {
            line.push(audio::clock(duration));
        }
        match facts.pages {
            Some(1) => line.push("1 page".into()),
            Some(pages) => line.push(format!("{pages} pages")),
            None => {}
        }
        if let Some(tags) = &facts.tags {
            if let Some(duration) = tags.duration {
                line.push(audio::clock(duration));
            }
            if let Some(rate) = tags.bitrate {
                line.push(format!("{rate} kbit/s"));
            }
            if let Some(rate) = tags.sample_rate {
                line.push(format!("{:.1} kHz", rate as f32 / 1000.));
            }
        }
        if let Some(size) = self.size {
            line.push(crate::fs::format_size(size));
        }
        line.join(" · ")
    }

    /// An audio file's picture on screen: its pixels at the window's scale,
    /// held inside `COVER` on its long side.
    fn cover(&self, picture: &Picture) -> (f32, f32) {
        let scale = self.scale.max(1.);
        let (w, h) = (picture.width as f32 / scale, picture.height as f32 / scale);
        let fit = (COVER / w.max(h)).min(1.);
        (w * fit, h * fit)
    }
}

/// Where a picture of this shape goes in the card: the full width, unless it
/// is so tall that the height has to give, and then narrower and centered.
fn frame(aspect: f32) -> (f32, f32) {
    let aspect = if aspect.is_finite() && aspect > 0. {
        aspect
    } else {
        1.
    };
    let height = PICTURE / aspect;
    if height <= TALLEST {
        (PICTURE, height)
    } else {
        (TALLEST * aspect, TALLEST)
    }
}

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

/// Where a tooltip's picture is.
enum Shown<'a> {
    /// Being made. Its place is kept.
    Waiting,
    /// Will not come: no thumbnailer, or one that failed.
    Missing,
    Ready(&'a Picture),
}

impl<'a> Shown<'a> {
    fn of(preview: Option<&'a Preview>) -> Self {
        match preview {
            None => Shown::Waiting,
            Some(preview) => preview
                .picture
                .as_ref()
                .map_or(Shown::Missing, Shown::Ready),
        }
    }
}

impl PreviewTooltip {
    /// The picture of an image, a video or a PDF's first page, the full width
    /// of the card, or the place kept for it.
    fn picture_body(
        &self,
        facts: &Facts,
        preview: Option<&Preview>,
        colors: &theme::ThemeColors,
    ) -> gpui::AnyElement {
        let hold = colors.element_hover;
        match Shown::of(preview) {
            Shown::Ready(picture) => {
                let (w, h) = frame(picture.width as f32 / picture.height.max(1) as f32);
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .bg(hold)
                    .rounded_t(px(INNER_RADIUS))
                    .child(
                        img(picture.image.clone())
                            .w(px(w))
                            .h(px(h))
                            // Only a picture of the full width reaches the
                            // card's corners.
                            .when(w >= PICTURE, |img| img.rounded_t(px(INNER_RADIUS))),
                    )
                    .into_any_element()
            }
            Shown::Missing => note("No picture", colors),
            Shown::Waiting => {
                let (_, h) = frame(facts.aspect.unwrap_or(16. / 9.));
                placeholder(PICTURE, h, hold, |el| el.rounded_t(px(INNER_RADIUS)))
            }
        }
    }

    /// An audio file's tags, with its cover beside them when it has one.
    fn audio_body(
        &self,
        facts: &Facts,
        preview: Option<&Preview>,
        colors: &theme::ThemeColors,
    ) -> gpui::AnyElement {
        let tags = facts.tags.clone().unwrap_or_default();
        let cover = match Shown::of(preview) {
            Shown::Ready(picture) => {
                let (w, h) = self.cover(picture);
                Some(
                    img(picture.image.clone())
                        .flex_none()
                        .w(px(w))
                        .h(px(h))
                        .rounded_sm()
                        .into_any_element(),
                )
            }
            Shown::Missing => None,
            // `aspect` is set only for a file that has a cover to show.
            Shown::Waiting => facts.aspect.map(|_| {
                placeholder(COVER, COVER, colors.element_hover, |el| {
                    el.flex_none().rounded_sm()
                })
            }),
        };
        div()
            .flex()
            .flex_row()
            .gap_2()
            .children(cover)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .when_some(tags.title, |col, title| {
                        col.child(div().truncate().child(title))
                    })
                    .when_some(tags.artist, |col, artist| {
                        col.child(div().truncate().child(artist))
                    })
                    .when_some(tags.album, |col, album| {
                        col.child(div().truncate().text_color(colors.text_muted).child(album))
                    }),
            )
            .into_any_element()
    }
}

/// A text file's first lines, in the mono font.
fn text_body(
    facts: &Facts,
    mono: Option<SharedString>,
    colors: &theme::ThemeColors,
) -> gpui::AnyElement {
    match &facts.lines {
        Some(lines) if !lines.is_empty() => div()
            .flex()
            .flex_col()
            .text_xs()
            .when_some(mono, |el, family| el.font_family(family))
            .children(lines.iter().map(|line| {
                // An empty line would collapse to no height.
                let line = if line.is_empty() { " " } else { line.as_str() };
                div()
                    .whitespace_nowrap()
                    .child(SharedString::from(line.to_string()))
            }))
            .into_any_element(),
        Some(_) => note("Empty file", colors),
        None => note("Not text", colors),
    }
}

/// A short muted line in place of a body.
fn note(text: &'static str, colors: &theme::ThemeColors) -> gpui::AnyElement {
    div()
        .text_color(colors.text_muted)
        .child(text)
        .into_any_element()
}

impl Render for PreviewTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Until the kind is known, and for a file with nothing to show, an
        // empty element: gpui draws a tooltip at whatever size its view takes.
        let known = loader::known(&self.key, cx);
        let Some(facts) = known.facts().cloned().filter(|f| f.kind != Kind::Other) else {
            return div().into_any_element();
        };
        let preview = known.done().cloned();
        let preview = preview.as_deref();
        let mono = crate::theming::numeric_font(cx);
        let ui = crate::theming::ui_font(cx);
        let colors = cx.theme().colors();

        let body = match facts.kind {
            Kind::Image | Kind::Video | Kind::Document => {
                self.picture_body(&facts, preview, colors)
            }
            Kind::Audio => self.audio_body(&facts, preview, colors),
            Kind::Text => text_body(&facts, mono, colors),
            Kind::Other => unreachable!("filtered above"),
        };
        // An image or a video fills the card's width with its picture, or
        // with the place kept for it. The card then has a fixed width, and
        // only the facts line under the picture is padded.
        let full_bleed = matches!(facts.kind, Kind::Image | Kind::Video | Kind::Document)
            && !matches!(Shown::of(preview), Shown::Missing);
        let line = self.facts_line(&facts, preview);

        // The padding keeps the box clear of the pointer: gpui puts a
        // tooltip one pixel from it.
        div()
            .pl_2()
            .pt_2p5()
            .child(
                div()
                    // The picture is the top of the card, so there is no
                    // padding above it.
                    .when(full_bleed, |el| el.w(px(PICTURE + 2.)).pb_2())
                    .when(!full_bleed, |el| el.max_w(px(MAX_WIDTH)).p_2())
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .shadow_md()
                    .text_sm()
                    .text_color(colors.text)
                    // Drawn at the window's root, outside the workspace's
                    // tree, so the family is not inherited from there.
                    .when_some(ui, |el, family| el.font_family(family))
                    .child(body)
                    .when(!line.is_empty(), |el| {
                        el.child(
                            div()
                                .when(full_bleed, |el| el.px_2())
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child(line),
                        )
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(mimes: &[&str]) -> Vec<String> {
        mimes.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn kinds_follow_the_type_and_its_ancestors() {
        assert_eq!(Kind::of(&chain(&["image/png"])), Kind::Image);
        assert_eq!(Kind::of(&chain(&["video/mp4"])), Kind::Video);
        assert_eq!(Kind::of(&chain(&["audio/flac"])), Kind::Audio);
        assert_eq!(Kind::of(&chain(&["text/rust", "text/plain"])), Kind::Text);
        assert_eq!(
            Kind::of(&chain(&[
                "application/json",
                "application/javascript",
                "text/plain"
            ])),
            Kind::Text
        );
        assert_eq!(Kind::of(&chain(&["application/pdf"])), Kind::Document);
        assert_eq!(Kind::of(&chain(&["application/zip"])), Kind::Other);
        assert_eq!(Kind::of(&[]), Kind::Other);
    }

    #[test]
    fn real_files_are_sniffed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("picture");
        // No extension: the magic has to find it.
        let mut bytes = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(&png, bytes).unwrap();
        assert_eq!(facts(&png).kind, Kind::Image);
        assert_eq!(facts(&png).dimensions, Some((2, 2)));

        let source = dir.path().join("main.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        assert_eq!(facts(&source).kind, Kind::Text);
    }
}
