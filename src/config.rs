//! Where hoja keeps things, and what it keeps there.
//!
//! Three directories, split by who writes them:
//!
//! - `~/.config/hoja/` — **you** write this. `settings.json` and `themes/`.
//!   hoja never writes here, so it cannot reformat the file, drop the comments,
//!   or lose an edit made while it was running.
//! - `~/.local/state/hoja/` — **hoja** writes this. Column widths, window size,
//!   and the view settings you last toggled. Things nobody would author by
//!   hand, which is exactly what XDG_STATE_HOME is for.
//! - `~/.local/share/hoja/` — the command palette's recency counts.
//!
//! Settings seed, state remembers. A new install takes its defaults from
//! `settings.json`; toggling hidden files in the menu records the new value in
//! state, so it survives a restart. Editing `settings.json` wins, because the
//! file is watched and a change applies over whatever was remembered — the
//! hand-written answer is always the one that takes effect last.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, prelude::*};
use serde::{Deserialize, Serialize};

use crate::fs::{Sort, SortDir, SortKey, ViewSettings};

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    base("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
pub fn state_home() -> Option<PathBuf> {
    base("XDG_STATE_HOME", ".local/state")
}

/// The spec says a relative value is invalid and must be ignored, which is why
/// this filters rather than trusting the variable.
fn base(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
}

pub fn settings_file() -> Option<PathBuf> {
    Some(config_home()?.join("hoja/settings.json"))
}

pub fn state_file() -> Option<PathBuf> {
    Some(state_home()?.join("hoja/state.json"))
}

// ---------------------------------------------------------------------------

/// What you write. Every field is optional so a partial file is valid — writing
/// only `{"theme": "…"}` should not reset everything else to a default.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub theme: Option<String>,
    pub view: ViewDefaults,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewDefaults {
    pub sort: Option<SortSetting>,
    pub show_hidden: Option<bool>,
    pub folders_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SortSetting {
    pub key: SortKeyName,
    pub direction: SortDirName,
}

impl Default for SortSetting {
    fn default() -> Self {
        let sort = Sort::default();
        Self {
            key: sort.key.into(),
            direction: sort.dir.into(),
        }
    }
}

/// Spelled out in the file rather than reusing the internal enums, so the
/// on-disk names are a deliberate choice and renaming a variant cannot silently
/// invalidate everyone's configuration.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKeyName {
    Name,
    Size,
    Kind,
    Modified,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirName {
    Ascending,
    Descending,
}

impl From<SortKey> for SortKeyName {
    fn from(key: SortKey) -> Self {
        match key {
            SortKey::Name => Self::Name,
            SortKey::Size => Self::Size,
            SortKey::Kind => Self::Kind,
            SortKey::Modified => Self::Modified,
        }
    }
}

impl From<SortKeyName> for SortKey {
    fn from(key: SortKeyName) -> Self {
        match key {
            SortKeyName::Name => Self::Name,
            SortKeyName::Size => Self::Size,
            SortKeyName::Kind => Self::Kind,
            SortKeyName::Modified => Self::Modified,
        }
    }
}

impl From<SortDir> for SortDirName {
    fn from(dir: SortDir) -> Self {
        match dir {
            SortDir::Ascending => Self::Ascending,
            SortDir::Descending => Self::Descending,
        }
    }
}

impl From<SortDirName> for SortDir {
    fn from(dir: SortDirName) -> Self {
        match dir {
            SortDirName::Ascending => Self::Ascending,
            SortDirName::Descending => Self::Descending,
        }
    }
}

impl Settings {
    /// Read the file, or return defaults.
    ///
    /// A malformed file says so and is then ignored: refusing to start because
    /// of a stray comma would be a poor trade, and the message is enough to fix
    /// it. `deny_unknown_fields` means a typo is reported rather than silently
    /// doing nothing.
    pub fn load() -> Self {
        let Some(path) = settings_file() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json_lenient::from_str(&text) {
            Ok(settings) => settings,
            Err(err) => {
                eprintln!("[hoja] {}: {err}", path.display());
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// What hoja writes. Column widths and window size have no hand-authored form;
/// the view settings are here because a toggle should survive a restart.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub sort: Option<SortSetting>,
    pub show_hidden: Option<bool>,
    pub folders_first: Option<bool>,
    /// Keyed by column name so reordering or inserting a column cannot shift
    /// everyone's saved widths onto the wrong ones.
    pub column_widths: std::collections::HashMap<String, f32>,
    pub window: Option<WindowState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
}

impl State {
    pub fn load() -> Self {
        let Some(path) = state_file() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        // A corrupt state file is not worth a message: nobody wrote it by hand,
        // and the defaults are perfectly usable.
        serde_json_lenient::from_str(&text).unwrap_or_default()
    }

    /// Write on a background thread. Called whenever a remembered value changes,
    /// which for a column drag is every frame, so the write is coalesced by the
    /// caller rather than throttled here.
    pub fn save(&self, cx: &App) {
        let Some(path) = state_file() else {
            return;
        };
        let Ok(text) = serde_json_lenient::to_string_pretty(self) else {
            return;
        };
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, text);
        })
        .detach();
    }
}

/// The view a new pane starts with: what you configured, overridden by what you
/// last toggled.
pub fn initial_view(settings: &Settings, state: &State) -> ViewSettings {
    let default = ViewSettings::default();
    let sort = state
        .sort
        .or(settings.view.sort)
        .map(|s| Sort {
            key: s.key.into(),
            dir: s.direction.into(),
        })
        .unwrap_or(default.sort);

    ViewSettings {
        sort,
        show_hidden: state
            .show_hidden
            .or(settings.view.show_hidden)
            .unwrap_or(default.show_hidden),
        folders_first: state
            .folders_first
            .or(settings.view.folders_first)
            .unwrap_or(default.folders_first),
    }
}

/// How long to wait before writing state after a change, so dragging a divider
/// writes once rather than once per frame.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Whether a watch event means something actually changed.
///
/// Load-bearing, and not obvious: on Linux, *reading* a watched file or listing
/// a watched directory emits `Access(Open)`. A watcher that reacts to every
/// event and then reads what it watches therefore wakes itself up forever — the
/// read is indistinguishable from an edit. Every watcher here reads what it
/// watches, so every one of them needs this.
pub fn is_content_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Drain a watcher channel, reporting whether any real change arrived.
pub fn drain_changes(rx: &std::sync::mpsc::Receiver<notify::Result<notify::Event>>) -> bool {
    let mut changed = false;
    while let Ok(event) = rx.try_recv() {
        if event.is_ok_and(|event| is_content_change(&event)) {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_leaves_the_rest_alone() {
        let settings: Settings = serde_json_lenient::from_str(r#"{"theme": "Rosé Pine"}"#).unwrap();
        assert_eq!(settings.theme.as_deref(), Some("Rosé Pine"));
        assert!(settings.view.show_hidden.is_none(), "unset stays unset");

        let view = initial_view(&settings, &State::default());
        assert_eq!(view, ViewSettings::default(), "and falls back to the default");
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed() {
        let settings: Settings = serde_json_lenient::from_str(
            r#"{
                // The theme, by name.
                "view": { "show_hidden": true, },
            }"#,
        )
        .unwrap();
        assert_eq!(settings.view.show_hidden, Some(true));
    }

    #[test]
    fn state_overrides_settings_but_only_where_it_has_an_answer() {
        let settings: Settings = serde_json_lenient::from_str(
            r#"{"view": {"show_hidden": false, "folders_first": false}}"#,
        )
        .unwrap();
        let state = State {
            show_hidden: Some(true),
            ..State::default()
        };

        let view = initial_view(&settings, &state);
        assert!(view.show_hidden, "the toggle you last flipped wins");
        assert!(!view.folders_first, "what you never toggled stays configured");
    }

    #[test]
    fn a_typo_is_reported_rather_than_ignored() {
        // deny_unknown_fields: a misspelled key is a mistake worth surfacing,
        // not a line that silently does nothing.
        let result: Result<Settings, _> =
            serde_json_lenient::from_str(r#"{"view": {"show_hiden": true}}"#);
        assert!(result.is_err());
    }

    #[test]
    fn sort_names_round_trip_through_their_on_disk_spelling() {
        let text = serde_json_lenient::to_string(&SortSetting {
            key: SortKey::Modified.into(),
            direction: SortDir::Descending.into(),
        })
        .unwrap();
        assert!(text.contains("\"modified\""), "got {text}");
        assert!(text.contains("\"descending\""), "got {text}");

        let back: SortSetting = serde_json_lenient::from_str(&text).unwrap();
        assert!(matches!(SortKey::from(back.key), SortKey::Modified));
        assert!(matches!(SortDir::from(back.direction), SortDir::Descending));
    }
}
