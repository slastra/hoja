use std::borrow::Cow;

use anyhow::Context as _;
use gpui::{AssetSource, SharedString};
use rust_embed::RustEmbed;

/// Serves bundled assets to GPUI. `svg()` resolves its `.path(...)` through this, and
/// `ThemeRegistry` uses `list()` to discover bundled themes.
///
/// Icons under `assets/icons/file_icons/` are Lucide-derived and ISC licensed, bar
/// the two drawn here; see `assets/icons/LICENSES`, which is embedded alongside them
/// so the notice ships with any binary that includes the art.
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*"]
#[include = "themes/**/*"]
#[exclude = "*.DS_Store"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Self::get(path)
            .map(|file| Some(file.data))
            .with_context(|| format!("loading asset at path {path:?}"))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
