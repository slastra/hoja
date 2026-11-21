//! File icon themes, as Zed has them.
//!
//! Zed's own file icons are one colour, which is what hoja draws by default.
//! Coloured icons come from an icon theme: a JSON file naming an SVG per file
//! type, shipped in a Zed extension. hoja loads the icon themes of every Zed
//! extension installed on this machine, and those under
//! `$XDG_CONFIG_HOME/hoja/icon_themes/<name>/`, laid out as an extension is:
//! `icon_themes/*.json`, with icon paths relative to `<name>/`.
//!
//! Which one is used:
//!
//! 1. `icon_theme` in hoja's settings.json, when it is set.
//! 2. Otherwise `icon_theme` in Zed's own settings.json, so hoja matches the
//!    editor next to it without being told twice.
//! 3. Otherwise Zed's default set, the monochrome one.
//!
//! A name can be a theme ("Colored Zed Icons Theme Dark") or the family it
//! belongs to ("Colored Zed Icons Theme"). A family takes the variant whose
//! appearance matches the colour theme, and follows it when that changes. As
//! in Zed, the setting can also give one name for light and one for dark.
//!
//! The icon theme's SVGs keep their own colours: `Icon` draws a path outside
//! `icons/` through `img()`, not through the tinting `svg()`. A file type the
//! theme does not cover falls back to the default set's icon, in the content
//! colour, which is how Zed does it.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Global};
use serde::Deserialize;
use theme::{Appearance, GlobalTheme, IconTheme, ThemeRegistry};

/// The `icon_theme` setting, in either of Zed's two shapes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IconThemeSetting {
    /// A theme, or a family that follows the colour theme's appearance.
    Name(String),
    /// One for each appearance. `mode` is read and ignored, so a block copied
    /// from Zed's settings is accepted as it is.
    Modes {
        #[serde(default)]
        #[allow(dead_code)]
        mode: Option<String>,
        light: Option<String>,
        dark: Option<String>,
    },
}

impl IconThemeSetting {
    /// The name this asks for under `appearance`.
    fn name(&self, appearance: Appearance) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name),
            Self::Modes { light, dark, .. } => match appearance {
                Appearance::Light => light.as_deref(),
                Appearance::Dark => dark.as_deref(),
            },
        }
    }
}

/// What hoja's settings asked for, kept so a change of colour theme can pick
/// the matching variant again.
#[derive(Default)]
struct Choice(Option<IconThemeSetting>);

impl Global for Choice {}

/// Register every icon theme found on disk. Errors are reported and skipped:
/// one broken extension must not cost the others.
pub fn load(cx: &mut App) {
    let registry = ThemeRegistry::global(cx);
    for root in roots() {
        let Ok(files) = std::fs::read_dir(root.join("icon_themes")) else {
            continue;
        };
        for file in files.flatten().map(|f| f.path()) {
            if file.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            if let Err(err) = registry.load_icon_theme(&file, &root, bytes) {
                eprintln!("[hoja] icon theme {} not loaded: {err:#}", file.display());
            }
        }
    }
}

/// Directories laid out as a Zed extension that may hold icon themes.
fn roots() -> Vec<PathBuf> {
    let mut parents = Vec::new();
    if let Some(data) = crate::config::data_home() {
        parents.push(data.join("zed/extensions/installed"));
    }
    if let Some(config) = crate::config::config_home() {
        parents.push(config.join("hoja/icon_themes"));
    }
    let mut roots: Vec<PathBuf> = parents
        .iter()
        .filter_map(|parent| std::fs::read_dir(parent).ok())
        .flat_map(|dirs| dirs.flatten().map(|d| d.path()))
        .filter(|path| path.is_dir())
        .collect();
    roots.sort();
    roots
}

/// Use the icon theme `setting` names, or Zed's choice when it names none.
pub fn apply(setting: Option<IconThemeSetting>, cx: &mut App) {
    cx.set_global(Choice(setting));
    reapply(cx);
}

/// Pick the icon theme again for the colour theme now in force. Called when
/// the colour theme changes, since a family follows its appearance.
pub fn reapply(cx: &mut App) {
    let Some(choice) = cx.try_global::<Choice>() else {
        // Before `apply`: the startup theme is set before icon themes load.
        return;
    };
    let setting = choice.0.clone().or_else(zed_choice);
    let appearance = GlobalTheme::theme(cx).appearance;
    let registry = ThemeRegistry::global(cx);
    let chosen = setting
        .as_ref()
        .and_then(|setting| setting.name(appearance))
        .and_then(|name| {
            let found = find(&registry, name, appearance);
            if found.is_none() {
                eprintln!("[hoja] icon theme {name:?} not found");
            }
            found
        })
        .or_else(|| registry.default_icon_theme().ok());
    let Some(chosen) = chosen else {
        return;
    };
    if GlobalTheme::icon_theme(cx).id != chosen.id {
        GlobalTheme::update_icon_theme(cx, chosen);
        cx.refresh_windows();
    }
}

/// A theme by its name, or failing that the variant of the family `name`
/// that matches `appearance`.
fn find(registry: &ThemeRegistry, name: &str, appearance: Appearance) -> Option<Arc<IconTheme>> {
    if let Ok(theme) = registry.get_icon_theme(name) {
        return Some(theme);
    }
    let metas = registry.list_icon_themes();
    let in_family = |meta: &&theme::ThemeMeta| meta.name.starts_with(name);
    let variant = metas
        .iter()
        .filter(in_family)
        .find(|meta| meta.appearance == appearance)
        .or_else(|| metas.iter().find(in_family))?;
    registry.get_icon_theme(&variant.name).ok()
}

/// `icon_theme` from Zed's settings.json, if Zed is configured here and sets
/// one. Only that key is read; the rest of the file is not hoja's business.
fn zed_choice() -> Option<IconThemeSetting> {
    let path = crate::config::config_home()?.join("zed/settings.json");
    zed_choice_in(&std::fs::read_to_string(path).ok()?)
}

fn zed_choice_in(text: &str) -> Option<IconThemeSetting> {
    let settings: serde_json_lenient::Value = serde_json_lenient::from_str(text).ok()?;
    serde_json_lenient::from_value(settings.get("icon_theme")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_of_the_setting_are_read() {
        let name: IconThemeSetting = serde_json_lenient::from_str(r#""Material""#).unwrap();
        assert_eq!(name.name(Appearance::Dark), Some("Material"));
        let modes: IconThemeSetting =
            serde_json_lenient::from_str(r#"{"mode": "system", "light": "Paper", "dark": "Ink"}"#)
                .unwrap();
        assert_eq!(modes.name(Appearance::Light), Some("Paper"));
        assert_eq!(modes.name(Appearance::Dark), Some("Ink"));
    }

    #[test]
    fn zeds_choice_is_read_from_its_settings() {
        let text = r#"{
            // Comments and trailing commas, as Zed allows.
            "theme": "Tint",
            "icon_theme": "Colored Zed Icons Theme",
        }"#;
        assert_eq!(
            zed_choice_in(text),
            Some(IconThemeSetting::Name("Colored Zed Icons Theme".into()))
        );
        assert_eq!(zed_choice_in(r#"{"theme": "Tint"}"#), None);
    }
}
