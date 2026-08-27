//! The chip that leaves the window, and the drag that carries it.
//!
//! gpui draws a preview inside its own window and hands a drag that leaves to
//! the compositor with no icon, so a drag out of hoja used to carry nothing.
//! `hoja-drag` takes the outbound drag over instead. This module is the seam:
//! it owns the one `DragSource`, turns a `ChipSpec` into pixels, and keeps the
//! *description* of the chip in one place so the gpui element drawn in-window
//! and the icon drawn outside cannot drift apart.
//!
//! A drag is armed when it begins and launched when the pointer leaves the
//! viewport, which is the same moment gpui would have promoted it. Arming early
//! is not an optimisation: the label and the theme are only reachable from the
//! row that started the drag.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use gpui::{Hsla, Rgba, SharedString, SvgRenderer, Window};

/// Everything the chip shows. One description, two renderers: `DragPreview`
/// draws it as a gpui element, `svg` rasterises it for the icon surface.
#[derive(Clone)]
pub struct ChipSpec {
    pub label: SharedString,
    /// A folder glyph only for a lone directory; a mixed or multiple selection
    /// gets the neutral one.
    pub folder: bool,
    pub background: Hsla,
    pub foreground: Hsla,
    pub border: Hsla,
}

/// Geometry shared by both renderers, so the two boxes are the same size.
pub mod chip {
    pub const HEIGHT: f32 = 40.;
    pub const PAD: f32 = 12.;
    pub const GLYPH: f32 = 18.;
    pub const GAP: f32 = 9.;
    pub const FONT: f32 = 13.5;
    pub const RADIUS: f32 = 6.;
    /// Where the chip sits relative to the pointer. Below and right, the same
    /// in the window and out of it, so crossing the edge changes nothing.
    pub const OFFSET: (f32, f32) = (14., 10.);
}

static SOURCE: OnceLock<Option<hoja_drag::DragSource>> = OnceLock::new();
static ARMED: Mutex<Option<(hoja_drag::Chip, Vec<String>)>> = Mutex::new(None);
/// Whether the compositor is currently carrying a chip of ours.
static LAUNCHED: AtomicBool = AtomicBool::new(false);

/// Attach to the window's Wayland connection, once.
///
/// Safe to call on any platform: without Wayland handles this records that
/// there is no source and everything falls back to gpui's own promotion.
pub fn attach(window: &Window) {
    SOURCE.get_or_init(|| {
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

        // `Window::window_handle` is also a gpui concept, hence the long form.
        let surface = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(raw_window_handle::RawWindowHandle::Wayland(h)) => h.surface.as_ptr(),
            _ => return None,
        };
        let display = match HasDisplayHandle::display_handle(window).map(|h| h.as_raw()) {
            Ok(raw_window_handle::RawDisplayHandle::Wayland(h)) => h.display.as_ptr(),
            _ => return None,
        };
        // SAFETY: both pointers come from the window we were handed, and the
        // window outlives the process's drag source.
        unsafe { hoja_drag::DragSource::attach(display, surface) }
    });
}

/// Whether hoja owns its outbound drags. When false the caller must leave
/// gpui's `external_drag_payload` in place, or dragging out stops working.
pub fn available() -> bool {
    SOURCE.get().is_some_and(Option::is_some)
}

/// Hold a chip ready for the moment the pointer leaves the window.
///
/// A drag that never leaves simply leaves its chip armed; the next drag
/// overwrites it, so there is nothing to tidy up.
pub fn arm(spec: &ChipSpec, paths: &[std::path::PathBuf]) {
    if !available() {
        return;
    }
    let Some(chip) = rasterise(spec) else { return };
    let uris = paths.iter().map(|p| file_uri(p)).collect();
    LAUNCHED.store(false, Ordering::Relaxed);
    *ARMED.lock().unwrap() = Some((chip, uris));
}

/// Start the compositor drag. Returns false when there was nothing armed, or
/// the compositor refused, in which case the drag simply stays inside.
pub fn launch() -> bool {
    let Some(Some(source)) = SOURCE.get() else {
        return false;
    };
    let Some((chip, uris)) = ARMED.lock().unwrap().take() else {
        return false;
    };
    let started = source.start(chip, &uris);
    LAUNCHED.store(started, Ordering::Relaxed);
    started
}

/// Whether the compositor is drawing our chip right now.
///
/// True from the moment the drag leaves until it ends, *including* while the
/// pointer is back over hoja's own window: a drag icon follows the cursor
/// everywhere, over our surface as much as anybody else's. So this is what
/// stops `DragPreview` painting a second chip on top of the first.
pub fn launched() -> bool {
    LAUNCHED.load(Ordering::Relaxed)
}

/// The next drag-end event, if any. Never blocks.
pub fn poll() -> Option<hoja_drag::DragEvent> {
    let event = match SOURCE.get() {
        Some(Some(source)) => source.poll(),
        _ => None,
    };
    if event.is_some() {
        LAUNCHED.store(false, Ordering::Relaxed);
    }
    event
}

/// Draw the chip to premultiplied BGRA.
///
/// `SvgRenderer` loads system fonts, so `<text>` rasterises with no font
/// machinery of our own, and its byte order is already what a little-endian
/// `Argb8888` buffer wants.
///
/// Its *alpha* is not. `swap_rgba_pa_to_bgra` divides each channel by the alpha
/// on the way out, so gpui hands back straight alpha while Wayland requires
/// premultiplied. The only partly transparent pixels in a chip are the
/// anti-aliased rounded corners and the outer edge of the stroke, so skipping
/// this does not look like a colour bug: it looks like the corner radius is
/// broken.
fn rasterise(spec: &ChipSpec) -> Option<hoja_drag::Chip> {
    let svg = svg(spec);
    let image = SvgRenderer::new(std::sync::Arc::new(()))
        .render_single_frame(svg.as_bytes(), 1.0)
        .ok()?;
    let size = image.size(0);
    let mut bgra = image.as_bytes(0)?.to_vec();
    premultiply(&mut bgra);
    Some(hoja_drag::Chip {
        bgra,
        width: size.width.0,
        height: size.height.0,
        scale: gpui::SMOOTH_SVG_SCALE_FACTOR as i32,
        hotspot: (chip::OFFSET.0 as i32, chip::OFFSET.1 as i32),
    })
}

/// Straight alpha to premultiplied, in place.
fn premultiply(bgra: &mut [u8]) {
    for px in bgra.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        // `+ 127` rounds rather than truncating; without it a long edge drifts
        // visibly darker than the fill it borders.
        for channel in &mut px[..3] {
            *channel = ((*channel as u32 * a + 127) / 255) as u8;
        }
    }
}

/// The chip as SVG.
///
/// No text measurement is available here, so the width is estimated from the
/// character count and the box is allowed to run wide. Too wide merely looks
/// roomy; too narrow would clip the name.
fn svg(spec: &ChipSpec) -> String {
    use chip::*;
    let glyph = if spec.folder {
        "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"
    } else {
        "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z M14 2v6h6"
    };
    let text_width = spec.label.chars().count() as f32 * FONT * 0.58;
    let width = PAD + GLYPH + GAP + text_width + PAD;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{HEIGHT}" viewBox="0 0 {width} {HEIGHT}">
  <rect x="0.75" y="0.75" width="{inner_w}" height="{inner_h}" rx="{RADIUS}"
        fill="{bg}" fill-opacity="{bg_a}" stroke="{border}" stroke-opacity="{border_a}" stroke-width="1.5"/>
  <g transform="translate({PAD} {glyph_y}) scale({glyph_scale})"
     fill="none" stroke="{fg}" stroke-width="2"
     stroke-linecap="round" stroke-linejoin="round">
    <path d="{glyph}"/>
  </g>
  <text x="{text_x}" y="{baseline}" font-family="Inter, DejaVu Sans, Liberation Sans, sans-serif"
        font-size="{FONT}" fill="{fg}">{label}</text>
</svg>"##,
        inner_w = width - 1.5,
        inner_h = HEIGHT - 1.5,
        glyph_y = (HEIGHT - GLYPH) / 2.,
        glyph_scale = GLYPH / 24.,
        text_x = PAD + GLYPH + GAP,
        baseline = HEIGHT / 2. + FONT * 0.36,
        bg = hex(spec.background),
        bg_a = Rgba::from(spec.background).a,
        border = hex(spec.border),
        border_a = Rgba::from(spec.border).a,
        fg = hex(spec.foreground),
        label = escape(&spec.label),
    )
}

fn hex(color: Hsla) -> String {
    let c = Rgba::from(color);
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02X}{:02X}{:02X}", byte(c.r), byte(c.g), byte(c.b))
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `file://` URI for a path, percent-encoding everything that is not an
/// unreserved character. Receivers split `text/uri-list` on CRLF and expect
/// each line to be a valid URI, so a name with a space or an accent has to be
/// encoded rather than passed through.
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn uris_encode_what_receivers_cannot_parse() {
        assert_eq!(file_uri(&PathBuf::from("/tmp/a.txt")), "file:///tmp/a.txt");
        assert_eq!(
            file_uri(&PathBuf::from("/tmp/two words.txt")),
            "file:///tmp/two%20words.txt"
        );
        // Non-ASCII goes out byte by byte, which is what UTF-8 percent-encoding is.
        assert_eq!(
            file_uri(&PathBuf::from("/tmp/café")),
            "file:///tmp/caf%C3%A9"
        );
        // A literal percent must not survive as one, or the receiver decodes a
        // sequence we never wrote.
        assert_eq!(file_uri(&PathBuf::from("/tmp/100%")), "file:///tmp/100%25");
    }

    #[test]
    fn markup_in_a_filename_cannot_break_the_svg() {
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(escape(r#"say "hi""#), "say &quot;hi&quot;");
        // The whole point: this must not close the text element.
        assert!(!escape("</text><script/>").contains('<'));
    }

    #[test]
    fn premultiplying_leaves_opaque_pixels_alone() {
        let mut px = [10, 20, 30, 255];
        premultiply(&mut px);
        assert_eq!(px, [10, 20, 30, 255]);
    }

    #[test]
    fn premultiplying_scales_by_alpha() {
        // Fully transparent: the colour cannot show through at all.
        let mut px = [200, 200, 200, 0];
        premultiply(&mut px);
        assert_eq!(px, [0, 0, 0, 0]);

        // Half transparent: half strength, rounded rather than truncated.
        let mut px = [200, 100, 51, 128];
        premultiply(&mut px);
        assert_eq!(px, [100, 50, 26, 128]);
    }

    #[test]
    fn colours_reach_the_svg_as_hex() {
        assert_eq!(hex(gpui::black()), "#000000");
        assert_eq!(hex(gpui::white()), "#FFFFFF");
    }
}
