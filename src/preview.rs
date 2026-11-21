//! Previews in a tooltip: the file under the pointer, without opening it.
//!
//! Four kinds of file get one: an image, a video, an audio file and a text
//! file. The picture for the first three comes from the freedesktop thumbnail
//! cache (see `thumbnail`), the tags of an audio file from `lofty`, and a text
//! file shows its first lines. Every other file, every folder and every member
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
mod text;
mod thumbnail;
mod video;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::AnimationExt as _;
use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EntityId, Global, IntoElement,
    ParentElement as _, Render, RenderImage, SharedString, Styled as _, WeakEntity, Window, div,
    img, prelude::FluentBuilder as _, px,
};
use theme::ActiveTheme as _;

use self::thumbnail::Size;

/// Previews kept, and so pictures kept in the atlas.
const KEEP: usize = 48;
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
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Subject {
    pub path: Arc<Path>,
    pub modified: Option<SystemTime>,
    pub size: Option<u64>,
}

/// The `tooltip` builder for a row.
pub fn tooltip(subject: Subject) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| PreviewTooltip::open(subject.clone(), window.scale_factor(), cx).into()
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
    let open = cx.try_global::<Previews>()?.open.as_ref()?;
    Some(open.1.clone())
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
// The cache

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    subject: Subject,
    size: Size,
}

#[derive(Default)]
struct Previews {
    ready: HashMap<Key, Arc<Preview>>,
    /// Oldest first.
    order: VecDeque<Key>,
    /// Work under way, and the tooltips waiting on it. One piece of work per
    /// file however many times it is hovered meanwhile. The facts, once
    /// they are in, so a tooltip that opens later still shows them.
    loading: HashMap<Key, Loading>,
    /// The tooltip on screen, by entity, with what the probe says about it.
    open: Option<(EntityId, String)>,
}

#[derive(Default)]
struct Loading {
    facts: Option<Arc<Facts>>,
    waiting: Vec<WeakEntity<PreviewTooltip>>,
}

impl Global for Previews {}

impl Previews {
    /// Keep `preview`, and give back the pictures of what it pushed out: an
    /// older preview of the same file at the same size, and the oldest ones
    /// past `KEEP`. Their textures are the caller's to drop.
    fn insert(&mut self, key: Key, preview: Arc<Preview>) -> Vec<Arc<RenderImage>> {
        let mut gone = Vec::new();
        if let Some(old) = self.ready.insert(key.clone(), preview) {
            self.order.retain(|k| k != &key);
            gone.push(old);
        }
        self.order.push_back(key);
        while self.order.len() > KEEP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            gone.extend(self.ready.remove(&oldest));
        }
        gone.iter()
            .filter_map(|preview| Some(preview.picture.as_ref()?.image.clone()))
            .collect()
    }

    fn touch(&mut self, key: &Key) {
        if let Some(at) = self.order.iter().position(|k| k == key) {
            let key = self.order.remove(at).expect("position is in range");
            self.order.push_back(key);
        }
    }
}

/// Hand the facts to the tooltips waiting on them.
fn learned(key: &Key, facts: Arc<Facts>, cx: &mut App) {
    let Some(loading) = cx.default_global::<Previews>().loading.get_mut(key) else {
        return;
    };
    loading.facts = Some(facts.clone());
    let waiting = loading.waiting.clone();
    for tooltip in waiting {
        let _ = tooltip.update(cx, |tooltip, cx| tooltip.learn(facts.clone(), cx));
    }
}

/// Put a finished preview in the cache, and drop what falls out of it.
fn finish(key: Key, preview: Arc<Preview>, cx: &mut App) {
    let previews = cx.default_global::<Previews>();
    let waiting = previews
        .loading
        .remove(&key)
        .map(|loading| loading.waiting)
        .unwrap_or_default();
    for image in previews.insert(key, preview.clone()) {
        cx.drop_image(image, None);
    }
    for tooltip in waiting {
        let _ = tooltip.update(cx, |tooltip, cx| tooltip.show(preview.clone(), cx));
    }
}

// ---------------------------------------------------------------------------
// The view

struct PreviewTooltip {
    name: SharedString,
    size: Option<u64>,
    /// The window's scale when the tooltip opened, which sets the size of the
    /// picture asked for and the size it is drawn at.
    scale: f32,
    facts: Option<Arc<Facts>>,
    /// Set when the picture is done, or known not to come.
    preview: Option<Arc<Preview>>,
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

        let previews = cx.default_global::<Previews>();
        let cached = previews.ready.get(&key).cloned();
        let known = cached
            .as_ref()
            .map(|preview| preview.facts.clone())
            .or_else(|| previews.loading.get(&key)?.facts.clone());
        let tooltip = cx.new(|_| PreviewTooltip {
            name: name.clone(),
            size: subject.size,
            scale,
            facts: known,
            preview: cached.clone(),
        });
        let id = tooltip.entity_id();
        let label = tooltip.read(cx).label();
        cx.default_global::<Previews>().open = Some((id, label));
        // Cleared when this tooltip goes, and only if a newer one has not
        // taken its place.
        //
        // gpui releases the view after the frame that stopped drawing it, and
        // the probe for that frame is already written by then, still naming
        // this tooltip. Nothing else asks for a frame, so ask for one, only
        // when a test is reading.
        let release = cx.observe_release(&tooltip, move |_, cx| {
            let previews = cx.default_global::<Previews>();
            if previews.open.as_ref().is_some_and(|(open, _)| *open == id) {
                previews.open = None;
                if crate::probe::path().is_some() {
                    cx.refresh_windows();
                }
            }
        });
        release.detach();

        if cached.is_some() {
            cx.default_global::<Previews>().touch(&key);
            return tooltip;
        }

        let previews = cx.default_global::<Previews>();
        if let Some(loading) = previews.loading.get_mut(&key) {
            loading.waiting.push(tooltip.downgrade());
            return tooltip;
        }
        previews.loading.insert(
            key.clone(),
            Loading {
                facts: None,
                waiting: vec![tooltip.downgrade()],
            },
        );

        cx.spawn(async move |cx| {
            let path = key.subject.path.clone();
            let facts = cx
                .background_spawn({
                    let path = path.clone();
                    async move { Arc::new(facts(&path)) }
                })
                .await;
            // The facts first, so the tooltip shows what it can, and keeps a
            // place of the right shape for the picture, while that is made.
            cx.update(|cx| learned(&key, facts.clone(), cx));
            let size = key.size;
            let preview = cx
                .background_spawn(async move { picture(&path, facts, size) })
                .await;
            cx.update(|cx| finish(key, Arc::new(preview), cx));
        })
        .detach();
        tooltip
    }

    /// What the probe says: the kind and the name, with `loading` in front
    /// until the work is done.
    fn label(&self) -> String {
        match (&self.preview, &self.facts) {
            (Some(preview), _) => format!("{}: {}", preview.facts.kind.name(), self.name),
            (None, Some(facts)) => format!("loading {}: {}", facts.kind.name(), self.name),
            (None, None) => format!("loading: {}", self.name),
        }
    }

    fn relabel(&self, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        let label = self.label();
        if let Some(open) = cx.default_global::<Previews>().open.as_mut()
            && open.0 == id
        {
            open.1 = label;
        }
    }

    fn learn(&mut self, facts: Arc<Facts>, cx: &mut Context<Self>) {
        self.facts = Some(facts);
        self.relabel(cx);
        cx.notify();
    }

    fn show(&mut self, preview: Arc<Preview>, cx: &mut Context<Self>) {
        self.facts = Some(preview.facts.clone());
        self.preview = Some(preview);
        self.relabel(cx);
        cx.notify();
    }

    /// The line under the body: dimensions, length and rates, then the size.
    fn facts_line(&self) -> String {
        let mut line = Vec::new();
        if let Some(facts) = &self.facts {
            let original = self.preview.as_ref().and_then(|p| p.original);
            if let Some((w, h)) = facts.dimensions.or(original) {
                line.push(format!("{w} × {h}"));
            }
            if let Some(duration) = facts.duration {
                line.push(audio::clock(duration));
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
/// is so tall that the height has to give, and then narrower and centred.
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

impl Render for PreviewTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Until the kind is known, and for a file with nothing to show, an
        // empty element: gpui draws a tooltip at whatever size its view takes.
        let Some(facts) = self.facts.clone().filter(|f| f.kind != Kind::Other) else {
            return div().into_any_element();
        };
        let kind = facts.kind;
        let colors = cx.theme().colors();
        let muted = colors.text_muted;
        let hold = colors.element_hover;
        let mono = crate::theming::numeric_font(cx);
        // `None` while the picture is being made, then `Some` of what came.
        let picture: Option<Option<&Picture>> = self
            .preview
            .as_ref()
            .map(|preview| preview.picture.as_ref());

        // An image or a video fills the card's width with its picture, or with
        // the place kept for it, so the card has a fixed width and only the
        // lines of text are padded.
        let full_bleed =
            matches!(kind, Kind::Image | Kind::Video) && !matches!(picture, Some(None));

        let body = match kind {
            Kind::Image | Kind::Video => match picture {
                Some(Some(picture)) => {
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
                                // Only a picture of the full width reaches
                                // the card's corners.
                                .when(w >= PICTURE, |img| img.rounded_t(px(INNER_RADIUS))),
                        )
                        .into_any_element()
                }
                Some(None) => div()
                    .text_color(muted)
                    .child("No picture")
                    .into_any_element(),
                None => {
                    let (_, h) = frame(facts.aspect.unwrap_or(16. / 9.));
                    placeholder(PICTURE, h, hold, |el| el.rounded_t(px(INNER_RADIUS)))
                }
            },
            Kind::Audio => {
                let tags = facts.tags.clone().unwrap_or_default();
                let cover = match picture {
                    Some(Some(picture)) => {
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
                    Some(None) => None,
                    None => facts
                        .aspect
                        .map(|_| placeholder(COVER, COVER, hold, |el| el.flex_none().rounded_sm())),
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
                                col.child(div().truncate().text_color(muted).child(album))
                            }),
                    )
                    .into_any_element()
            }
            Kind::Text => match &facts.lines {
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
                Some(_) => div()
                    .text_color(muted)
                    .child("Empty file")
                    .into_any_element(),
                None => div().text_color(muted).child("Not text").into_any_element(),
            },
            Kind::Other => unreachable!("filtered above"),
        };

        let line = self.facts_line();
        let padded = |el: gpui::Div| if full_bleed { el.px_2() } else { el };

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
                    .when_some(crate::theming::ui_font(cx), |el, family| {
                        el.font_family(family)
                    })
                    .child(body)
                    .when(!line.is_empty(), |el| {
                        el.child(padded(div().text_xs().text_color(muted).child(line)))
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
        assert_eq!(Kind::of(&chain(&["application/zip"])), Kind::Other);
        assert_eq!(Kind::of(&[]), Kind::Other);
    }

    fn preview_of(n: u64) -> (Key, Arc<Preview>) {
        let key = Key {
            subject: Subject {
                path: Path::new("/f").join(n.to_string()).into(),
                modified: None,
                size: Some(n),
            },
            size: Size::Large,
        };
        let facts = Facts {
            kind: Kind::Text,
            mimes: Vec::new(),
            dimensions: None,
            aspect: None,
            duration: None,
            lines: None,
            tags: None,
        };
        let preview = Preview {
            facts: Arc::new(facts),
            picture: None,
            original: None,
        };
        (key, Arc::new(preview))
    }

    #[test]
    fn the_cache_keeps_the_newest() {
        let mut previews = Previews::default();
        for n in 0..KEEP as u64 + 2 {
            let (key, preview) = preview_of(n);
            previews.insert(key, preview);
        }
        assert_eq!(previews.ready.len(), KEEP);
        assert_eq!(previews.order.len(), KEEP);
        assert!(!previews.ready.contains_key(&preview_of(0).0));
        assert!(!previews.ready.contains_key(&preview_of(1).0));
        assert!(previews.ready.contains_key(&preview_of(2).0));

        // The same file again replaces its entry rather than taking a second
        // place, and becomes the newest.
        let (key, preview) = preview_of(2);
        previews.insert(key.clone(), preview);
        assert_eq!(previews.order.len(), KEEP);
        assert!(previews.order.back() == Some(&key));
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
