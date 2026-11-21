//! The hover preview of a row: the tooltip view.

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use theme::ActiveTheme as _;

use super::facts::{Facts, Kind, Picture, Preview};
use super::loader::{self, Key, Known, Need};
use super::thumbnail::Size;
use super::{INNER_RADIUS, Subject, audio, placeholder};

/// The width of an image's or a video's picture, in logical pixels.
const PICTURE: f32 = 256.;
/// The tallest a picture is drawn. A taller one is drawn narrower, so a
/// portrait phone video does not run off a laptop screen.
const TALLEST: f32 = 360.;
/// The long side of an audio file's picture, beside its tags.
const COVER: f32 = 96.;
/// The widest a tooltip grows, for a text file's long lines.
const MAX_WIDTH: f32 = 640.;

/// The open preview, for the probe: its kind and the file's name, with
/// `loading` in front until the picture is done.
pub fn probe(cx: &App) -> Option<String> {
    let (_, key, name) = cx.try_global::<OpenTooltip>()?.0.as_ref()?;
    Some(label(&loader::known(key, cx), name))
}

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

pub(super) struct PreviewTooltip {
    key: Key,
    size: Option<u64>,
    /// The window's scale when the tooltip opened, which sets the size of the
    /// picture asked for and the size it is drawn at.
    scale: f32,
}

impl PreviewTooltip {
    pub(super) fn open(subject: Subject, scale: f32, cx: &mut App) -> Entity<Self> {
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
    syntax: &theme::SyntaxTheme,
) -> gpui::AnyElement {
    match &facts.lines {
        Some(lines) if !lines.is_empty() => div()
            .flex()
            .flex_col()
            .text_xs()
            .when_some(mono, |el, family| el.font_family(family))
            .children(lines.iter().enumerate().map(|(ix, line)| {
                // An empty line would collapse to no height.
                let text = if line.is_empty() { " " } else { line.as_str() };
                // Each capture name to the theme's style for it, falling back
                // along its dots as Zed does: `function.method` to
                // `function`. A name the theme has no style for stays plain.
                let highlights: Vec<_> = facts
                    .highlights
                    .as_ref()
                    .and_then(|all| all.get(ix))
                    .into_iter()
                    .flatten()
                    .filter(|(range, _)| range.end <= text.len())
                    .filter_map(|(range, name)| {
                        let id = syntax.highlight_id(name)?;
                        Some((range.clone(), *syntax.get(id as usize)?))
                    })
                    .collect();
                div().whitespace_nowrap().child(
                    gpui::StyledText::new(SharedString::from(text.to_string()))
                        .with_highlights(highlights),
                )
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
        let syntax = cx.theme().syntax().clone();

        let body = match facts.kind {
            Kind::Image | Kind::Video | Kind::Document => {
                self.picture_body(&facts, preview, colors)
            }
            Kind::Audio => self.audio_body(&facts, preview, colors),
            Kind::Text => text_body(&facts, mono, colors, &syntax),
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
                    // Nothing draws outside the card, whatever its width: a
                    // wider mono face than the one the line length was set
                    // for is clipped at the edge rather than spilling out.
                    .overflow_hidden()
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
