use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AssetSource};
use theme::{GlobalTheme, ThemeFamily, ThemeRegistry};
use theme_settings::{ThemeFamilyContent, refine_theme_family};

use crate::assets::Assets;

/// Where user themes live, mirroring Zed's `~/.config/zed/themes`.
pub fn themes_dir() -> PathBuf {
    crate::config::config_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hoja")
        .join("themes")
}

/// Parse one theme family file.
///
/// Lenient JSON on purpose: hand-edited themes routinely carry comments and trailing
/// commas, and Zed accepts those for user themes too. Every field in the format is
/// optional and there is no `deny_unknown_fields`, so a theme written for Zed loads
/// unmodified even though we only read a fraction of its ~190 colour tokens.
fn parse_family(bytes: &[u8]) -> Result<ThemeFamily> {
    let content: ThemeFamilyContent = serde_json_lenient::from_slice(bytes)?;
    Ok(refine_theme_family(content))
}

/// Load every `themes/**/*.json` embedded in the binary.
pub fn load_bundled_themes(cx: &mut App) -> Result<()> {
    let registry = ThemeRegistry::global(cx);

    for path in Assets.list("themes/")? {
        if !path.ends_with(".json") {
            continue;
        }
        let Some(bytes) = Assets.load(&path)? else {
            continue;
        };
        match parse_family(&bytes) {
            Ok(family) => registry.insert_theme_families([family]),
            Err(err) => eprintln!("[hoja] skipping bundled theme {path}: {err}"),
        }
    }
    Ok(())
}

/// Load user themes from `~/.config/pane/themes/*.json`, if the directory exists.
pub fn load_user_themes(cx: &mut App) {
    let dir = themes_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return; // Absent directory is the normal case, not an error.
    };
    let registry = ThemeRegistry::global(cx);

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path).map_err(Into::into).and_then(|bytes| {
            parse_family(&bytes).with_context(|| format!("parsing {}", path.display()))
        }) {
            Ok(family) => registry.insert_theme_families([family]),
            Err(err) => eprintln!("[hoja] skipping user theme {}: {err:#}", path.display()),
        }
    }
}

/// Switch the active theme by name. This is Zed's `reload_theme` in two lines, a global
/// swap plus a blunt full repaint, rather than per-view observation.
/// The theme that is actually on screen.
///
/// The watcher below needs to know this, and it cannot be told once: a theme
/// chosen through `settings.json` arrives long after the watcher is armed. A
/// watcher holding the *startup* name re-applies that name on every theme-file
/// change, so editing any theme file snapped the window back to whatever hoja
/// started with.
static ACTIVE: Mutex<Option<String>> = Mutex::new(None);

fn set_active(name: &str) {
    let mut active = ACTIVE.lock().unwrap_or_else(|err| err.into_inner());
    *active = Some(name.to_string());
}

fn active() -> Option<String> {
    ACTIVE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

pub fn apply(name: &str, cx: &mut App) -> Result<()> {
    let theme = ThemeRegistry::global(cx).get(name)?;
    GlobalTheme::update_theme(cx, theme);
    // Recorded only once it resolved, so a bad name in settings.json leaves the
    // working theme in place rather than pointing the watcher at nothing.
    set_active(name);
    cx.refresh_windows();
    Ok(())
}

pub fn list_names(cx: &mut App) -> Vec<String> {
    ThemeRegistry::global(cx)
        .list_names()
        .into_iter()
        .map(|n| n.to_string())
        .collect()
}

/// Watch the user theme directory and re-apply the current theme whenever a
/// file in it changes.
///
/// Only colour themes are watched, matching Zed: icon themes have no equivalent.
pub fn watch_user_themes(cx: &mut App) {
    let dir = themes_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("[hoja] theme watcher unavailable: {err}");
            return;
        }
    };

    use notify::Watcher as _;
    if let Err(err) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        eprintln!("[hoja] cannot watch {}: {err}", dir.display());
        return;
    }

    cx.spawn(async move |cx| {
        // Keep the watcher alive for the lifetime of this task.
        let _watcher = watcher;
        loop {
            // Poll rather than blocking on `recv`, so the receiver stays owned by this
            // task instead of being moved into a new future on every iteration.
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;

            if !crate::config::drain_changes(&rx) {
                continue;
            }

            // Coalesce the burst of events an editor emits when saving, then drain
            // whatever arrived during the wait.
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            crate::config::drain_changes(&rx);

            cx.update(|cx| {
                load_user_themes(cx);
                // Read now, not when the watcher was armed.
                let Some(active) = active() else {
                    return;
                };
                if let Err(err) = apply(&active, cx) {
                    eprintln!("[hoja] theme reload failed: {err}");
                }
            });
        }
    })
    .detach();
}

/// A monospaced family the system actually has, or `None` to leave numbers in
/// the UI font with tabular figures.
///
/// Everything that prints a figure uses this: the transfer strip, the Size and
/// Modified columns, and the pane footers. A size in the listing and the same
/// size in the footer below it are then the same shape, and a column of them
/// lines up digit under digit.
///
/// gpui matches a family name exactly and a miss loads nothing at all, so the
/// name cannot simply be asserted: generic aliases like "monospace" are a
/// fontconfig idea that never reaches the font database. The list is checked
/// against what is installed and the first hit wins.
pub fn numeric_font(cx: &App) -> Option<gpui::SharedString> {
    static CHOSEN: std::sync::OnceLock<Option<gpui::SharedString>> = std::sync::OnceLock::new();
    CHOSEN
        .get_or_init(|| {
            const PREFERRED: [&str; 8] = [
                "DejaVu Sans Mono",
                "Liberation Mono",
                "Noto Sans Mono",
                "JetBrains Mono",
                "Source Code Pro",
                "Fira Code",
                "Hack",
                "Ubuntu Mono",
            ];
            let installed = cx.text_system().all_font_names();
            PREFERRED
                .iter()
                .find(|name| installed.iter().any(|have| have == *name))
                .map(|name| gpui::SharedString::from(*name))
        })
        .clone()
}
