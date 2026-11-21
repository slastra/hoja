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

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EntityId, FontWeight, Global, IntoElement,
    ParentElement as _, Render, RenderImage, SharedString, Styled as _, WeakEntity, Window, div,
    img, prelude::FluentBuilder as _, px,
};
use theme::ActiveTheme as _;

use self::thumbnail::Size;

/// Previews kept, and so pictures kept in the atlas.
const KEEP: usize = 48;
/// The long side of a picture, in logical pixels.
const PICTURE: f32 = 256.;
/// The long side of an audio file's picture, beside its tags.
const COVER: f32 = 96.;
/// The widest a tooltip grows, which is where a long name truncates.
const MAX_WIDTH: f32 = 640.;

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

/// Whether rows get a preview, from the `previews` setting.
pub fn enabled(cx: &App) -> bool {
    cx.try_global::<Enabled>().is_none_or(|on| on.0)
}

/// `None` means the default, which is on.
pub fn set_enabled(on: Option<bool>, cx: &mut App) {
    cx.set_global(Enabled(on.unwrap_or(true)));
}

/// The open preview, for the probe: its kind and the file's name, or
/// `loading` before the kind is known.
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

struct Preview {
    kind: Kind,
    picture: Option<Picture>,
    /// Of the original, in pixels.
    dimensions: Option<(u32, u32)>,
    lines: Option<Vec<String>>,
    tags: Option<audio::Tags>,
}

/// The blocking half: what kind of file it is, then what it shows.
fn classify(path: &Path) -> (Kind, Vec<String>) {
    let mimes = crate::opener::mime_chain(path);
    (Kind::of(&mimes), mimes)
}

fn build(path: &Path, kind: Kind, mimes: &[String], size: Size) -> Preview {
    let picture = matches!(kind, Kind::Image | Kind::Video | Kind::Audio)
        .then(|| thumbnail::picture(path, mimes, size))
        .flatten();
    let dimensions = picture.as_ref().and_then(|p| p.original).or_else(|| {
        (kind == Kind::Image)
            .then(|| image::image_dimensions(path).ok())
            .flatten()
    });
    let picture = picture.and_then(|p| {
        let (width, height) = (p.width, p.height);
        let buffer = image::RgbaImage::from_raw(width, height, p.bgra)?;
        Some(Picture {
            image: Arc::new(RenderImage::new([image::Frame::new(buffer)])),
            width,
            height,
        })
    });
    Preview {
        kind,
        picture,
        dimensions,
        lines: (kind == Kind::Text).then(|| text::excerpt(path)).flatten(),
        tags: (kind == Kind::Audio).then(|| audio::read(path)).flatten(),
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
    /// file however many times it is hovered meanwhile.
    loading: HashMap<Key, Vec<WeakEntity<PreviewTooltip>>>,
    /// The tooltip on screen, by entity, with what the probe says about it.
    open: Option<(EntityId, String)>,
}

impl Global for Previews {}

impl Previews {
    fn touch(&mut self, key: &Key) {
        if let Some(at) = self.order.iter().position(|k| k == key) {
            let key = self.order.remove(at).expect("position is in range");
            self.order.push_back(key);
        }
    }
}

/// Put a finished preview in the cache, and drop what falls out of it.
fn finish(key: Key, preview: Arc<Preview>, cx: &mut App) {
    let previews = cx.default_global::<Previews>();
    let waiting = previews.loading.remove(&key).unwrap_or_default();
    if let Some(old) = previews.ready.insert(key.clone(), preview.clone()) {
        // The same file at the same size, made twice. Only the newer stays.
        previews.order.retain(|k| k != &key);
        if let Some(picture) = &old.picture {
            let image = picture.image.clone();
            cx.drop_image(image, None);
        }
    }
    let previews = cx.default_global::<Previews>();
    previews.order.push_back(key);
    let mut evicted = Vec::new();
    while previews.order.len() > KEEP {
        let Some(oldest) = previews.order.pop_front() else {
            break;
        };
        if let Some(gone) = previews.ready.remove(&oldest)
            && let Some(picture) = &gone.picture
        {
            evicted.push(picture.image.clone());
        }
    }
    for image in evicted {
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
    kind: Option<Kind>,
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

        let cached = cx.default_global::<Previews>().ready.get(&key).cloned();
        let tooltip = cx.new(|_| PreviewTooltip {
            name: name.clone(),
            size: subject.size,
            scale,
            kind: cached.as_ref().map(|p| p.kind),
            preview: cached.clone(),
        });
        let id = tooltip.entity_id();
        let label = match &cached {
            Some(preview) => format!("{}: {name}", preview.kind.name()),
            None => format!("loading: {name}"),
        };
        let previews = cx.default_global::<Previews>();
        previews.open = Some((id, label));
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
        if let Some(waiting) = previews.loading.get_mut(&key) {
            waiting.push(tooltip.downgrade());
            return tooltip;
        }
        previews
            .loading
            .insert(key.clone(), vec![tooltip.downgrade()]);

        cx.spawn(async move |cx| {
            let path = key.subject.path.clone();
            let (kind, mimes) = cx
                .background_spawn({
                    let path = path.clone();
                    async move { classify(&path) }
                })
                .await;
            // The kind first, so a file with nothing to show draws nothing
            // rather than a box that waits and then vanishes.
            cx.update(|cx| {
                let waiting = cx
                    .default_global::<Previews>()
                    .loading
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                for tooltip in waiting {
                    let _ = tooltip.update(cx, |tooltip, cx| {
                        tooltip.kind = Some(kind);
                        cx.notify();
                    });
                }
            });
            let size = key.size;
            let preview = cx
                .background_spawn(async move { build(&path, kind, &mimes, size) })
                .await;
            cx.update(|cx| finish(key, Arc::new(preview), cx));
        })
        .detach();
        tooltip
    }

    fn show(&mut self, preview: Arc<Preview>, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        if let Some(open) = cx.default_global::<Previews>().open.as_mut()
            && open.0 == id
        {
            open.1 = format!("{}: {}", preview.kind.name(), self.name);
        }
        self.kind = Some(preview.kind);
        self.preview = Some(preview);
        cx.notify();
    }

    /// The line under the body: dimensions or duration, then the size.
    fn facts(&self) -> String {
        let mut facts = Vec::new();
        if let Some(preview) = &self.preview {
            if let Some((w, h)) = preview.dimensions {
                facts.push(format!("{w} × {h}"));
            }
            if let Some(tags) = &preview.tags {
                if let Some(duration) = tags.duration {
                    facts.push(audio::clock(duration));
                }
                if let Some(rate) = tags.bitrate {
                    facts.push(format!("{rate} kbit/s"));
                }
                if let Some(rate) = tags.sample_rate {
                    facts.push(format!("{:.1} kHz", rate as f32 / 1000.));
                }
            }
        }
        if let Some(size) = self.size {
            facts.push(crate::fs::format_size(size));
        }
        facts.join(" · ")
    }

    /// The picture's size on screen: its pixels at the window's scale, held
    /// inside `limit` on its long side.
    fn drawn(&self, picture: &Picture, limit: f32) -> (f32, f32) {
        let scale = self.scale.max(1.);
        let (w, h) = (picture.width as f32 / scale, picture.height as f32 / scale);
        let fit = (limit / w.max(h)).min(1.);
        (w * fit, h * fit)
    }
}

impl Render for PreviewTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Until the kind is known, and for a file with nothing to show, an
        // empty element: gpui draws a tooltip at whatever size its view takes.
        let Some(kind) = self.kind.filter(|kind| *kind != Kind::Other) else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let muted = colors.text_muted;
        let mono = crate::theming::numeric_font(cx);
        let preview = self.preview.clone();

        let name = div()
            .truncate()
            .font_weight(FontWeight::MEDIUM)
            .child(self.name.clone());

        let body = match (kind, preview.as_deref()) {
            (_, None) => div().text_color(muted).child("…").into_any_element(),
            (Kind::Image | Kind::Video, Some(preview)) => match &preview.picture {
                // Edge to edge: the card is as wide as the picture and the
                // picture has no padding round it. A small original is drawn
                // larger to fill the width.
                Some(picture) => img(picture.image.clone())
                    .w_full()
                    .h(px(
                        PICTURE * picture.height as f32 / picture.width.max(1) as f32
                    ))
                    .into_any_element(),
                None => div()
                    .text_color(muted)
                    .child("No picture")
                    .into_any_element(),
            },
            (Kind::Audio, Some(preview)) => {
                let tags = preview.tags.clone().unwrap_or_default();
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .when_some(preview.picture.as_ref(), |row, picture| {
                        let (w, h) = self.drawn(picture, COVER);
                        row.child(
                            img(picture.image.clone())
                                .flex_none()
                                .w(px(w))
                                .h(px(h))
                                .rounded_sm(),
                        )
                    })
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
            (Kind::Text, Some(preview)) => match &preview.lines {
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
            (Kind::Other, _) => unreachable!("filtered above"),
        };

        let facts = self.facts();
        // An image or a video with a picture fills the card's width, so the
        // card has a fixed width and only the lines of text are padded.
        let full_bleed = matches!(kind, Kind::Image | Kind::Video)
            && preview.as_ref().is_some_and(|p| p.picture.is_some());
        let padded = |el: gpui::Div| if full_bleed { el.px_2() } else { el };

        // The padding keeps the box clear of the pointer: gpui puts a
        // tooltip one pixel from it.
        div()
            .pl_2()
            .pt_2p5()
            .child(
                div()
                    .when(full_bleed, |el| el.w(px(PICTURE + 2.)).py_2())
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
                    .child(padded(name))
                    .child(body)
                    .when(!facts.is_empty(), |el| {
                        el.child(padded(div().text_xs().text_color(muted).child(facts)))
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
        assert_eq!(classify(&png).0, Kind::Image);

        let source = dir.path().join("main.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        assert_eq!(classify(&source).0, Kind::Text);
    }
}
