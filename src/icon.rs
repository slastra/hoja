use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Hsla, IntoElement, Pixels, RenderOnce, SharedString, Window, img, prelude::*, px, svg,
};

/// Where an icon's bytes come from, which determines how it can be coloured.
///
/// GPUI's `svg()` rasterizes to an *alpha mask* and paints it as a solid quad in
/// `text_color`: every colour in the source SVG is discarded. That makes bundled
/// monochrome icons free to tint from the theme, but it also means a polychrome icon
/// from a third-party icon theme cannot use that path at all. Those go through `img()`,
/// which rasterizes via resvg and keeps its own colours.
///
/// Zed dispatches on an `"icons/"` path prefix; we match that so icon themes whose
/// `IconDefinition.path` was absolutized against a theme directory land on `img()`.
#[derive(Clone)]
enum IconSource {
    /// Monochrome SVG embedded in the binary, tinted by the theme.
    Embedded(SharedString),
    /// An icon-theme file on disk, rendered at its own colours.
    External(Arc<Path>),
}

#[derive(IntoElement, Clone)]
pub struct Icon {
    source: IconSource,
    size: Pixels,
    color: Hsla,
}

impl Icon {
    pub fn from_path(path: impl Into<SharedString>, color: Hsla) -> Self {
        let path = path.into();
        let source = if path.starts_with("icons/") {
            IconSource::Embedded(path)
        } else {
            IconSource::External(Arc::from(PathBuf::from(path.as_ref())))
        };

        Self {
            source,
            // 16px is the icons' native viewBox, so they rasterize without resampling.
            size: px(16.),
            color,
        }
    }

    #[allow(dead_code)] // part of the component API; rows use the default size
    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        match self.source {
            IconSource::Embedded(path) => svg()
                .size(self.size)
                .flex_none()
                .path(path)
                .text_color(self.color)
                .into_any_element(),
            IconSource::External(path) => {
                // `text_color` is a no-op here, the raster keeps its own colours.
                img(path).size(self.size).flex_none().into_any_element()
            }
        }
    }
}
