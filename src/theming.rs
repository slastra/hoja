use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AssetSource};
use theme::{GlobalTheme, ThemeFamily, ThemeRegistry};
use theme_settings::{ThemeFamilyContent, refine_theme_family};

use crate::assets::Assets;

/// Where user themes live, mirroring Zed's `~/.config/zed/themes`.
pub fn themes_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hoja").join("themes")
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

/// Switch the active theme by name. This is Zed's `reload_theme` in two lines — a global
/// swap plus a blunt full repaint, rather than per-view observation.
pub fn apply(name: &str, cx: &mut App) -> Result<()> {
    let theme = ThemeRegistry::global(cx).get(name)?;
    GlobalTheme::update_theme(cx, theme);
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

/// Watch the user theme directory and re-apply `active` whenever a file changes.
///
/// Only colour themes are watched, matching Zed — icon themes have no equivalent.
pub fn watch_user_themes(active: String, cx: &mut App) {
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

            let mut changed = false;
            while rx.try_recv().is_ok() {
                changed = true;
            }
            if !changed {
                continue;
            }

            // Coalesce the burst of events an editor emits when saving, then drain
            // whatever arrived during the wait.
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            while rx.try_recv().is_ok() {}

            let active = active.clone();
            cx.update(|cx| {
                load_user_themes(cx);
                if let Err(err) = apply(&active, cx) {
                    eprintln!("[hoja] theme reload failed: {err}");
                }
            });
        }
    })
    .detach();
}
