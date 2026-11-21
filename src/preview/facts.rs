//! What is quick to learn about a file, and its picture once made.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui::RenderImage;

use super::thumbnail::{self, Size};
use super::{audio, pdf, syntax, text, video};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
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
    pub(super) fn of(mimes: &[String]) -> Kind {
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

    pub(super) fn name(self) -> &'static str {
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

pub(super) struct Picture {
    pub(super) image: Arc<RenderImage>,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// What is quick to learn about a file: everything but its picture. Reading
/// it costs a header or the first 8 KiB, so it arrives well before a
/// thumbnailer has finished, and the tooltip shows it while it waits.
pub(super) struct Facts {
    pub(super) kind: Kind,
    pub(super) mimes: Vec<String>,
    /// Of the original as a viewer shows it, in pixels.
    pub(super) dimensions: Option<(u32, u32)>,
    /// Width over height of the picture to come, so its place can be kept
    /// at the right shape. `None` for a file that will have no picture.
    pub(super) aspect: Option<f32>,
    /// `aspect` was read from the file, rather than guessed for a format
    /// whose header could not be read. A measured shape is final; a guessed
    /// one waits for the picture to say.
    pub(super) measured: bool,
    /// Of a video. An audio file's is in its tags.
    pub(super) duration: Option<Duration>,
    /// Of a PDF, when `pdfinfo` could say.
    pub(super) pages: Option<u32>,
    pub(super) lines: Option<Vec<String>>,
    /// The lines' syntax highlights, by capture name, when a grammar knows
    /// the file. Coloured from the theme when drawn.
    pub(super) highlights: Option<Vec<syntax::LineHighlights>>,
    pub(super) tags: Option<audio::Tags>,
}

/// The facts and the picture, once the picture is done or has failed.
pub(super) struct Preview {
    pub(super) facts: Arc<Facts>,
    pub(super) picture: Option<Picture>,
    /// The original's size as the thumbnail records it. For a format whose
    /// header `image` cannot read, this is the only place it is known.
    pub(super) original: Option<(u32, u32)>,
}

/// The quick half, off the UI thread.
pub(super) fn read(path: &Path) -> Facts {
    let mimes = crate::opener::mime_chain(path);
    let kind = Kind::of(&mimes);
    let mut facts = Facts {
        kind,
        mimes,
        dimensions: None,
        aspect: None,
        measured: false,
        duration: None,
        pages: None,
        lines: None,
        highlights: None,
        tags: None,
    };
    match kind {
        Kind::Image => {
            facts.dimensions = image_size(path);
            facts.measured = facts.dimensions.is_some();
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
            facts.measured = video.is_some();
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
            facts.measured = pdf.is_some_and(|pdf| pdf.aspect.is_some());
            // A portrait Letter page when the first page cannot be measured.
            facts.aspect = Some(pdf.and_then(|pdf| pdf.aspect).unwrap_or(8.5 / 11.));
        }
        Kind::Text => {
            facts.lines = text::excerpt(path);
            facts.highlights = facts
                .lines
                .as_ref()
                .and_then(|lines| syntax::highlight(path, lines));
        }
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
pub(super) fn picture(path: &Path, facts: Arc<Facts>, size: Size) -> Preview {
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

/// The shape a finished preview's card keeps: the measured one from its
/// facts when there is one, which a thumbnail rounded to whole pixels cannot
/// disturb; the picture's own when the facts only guessed; none when there
/// is no picture to show.
pub(super) fn settled_shape(preview: &Preview) -> Option<f32> {
    let picture = preview.picture.as_ref()?;
    if preview.facts.measured {
        preview.facts.aspect
    } else {
        Some(picture.width as f32 / picture.height.max(1) as f32)
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
        assert_eq!(read(&png).kind, Kind::Image);
        assert_eq!(read(&png).dimensions, Some((2, 2)));

        let source = dir.path().join("main.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        assert_eq!(read(&source).kind, Kind::Text);
    }
}
