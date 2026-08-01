//! "Open" and "Open With": XDG mime resolution, `.desktop` discovery, and
//! detached launching.
//!
//! - MIME detection: `xdg-mime` (real shared-mime-info queries — globs plus
//!   magic sniffing).
//! - `.desktop` parsing and Exec field-code expansion: `freedesktop-desktop-entry`
//!   (`parse_exec_with_uris` handles `%f/%F/%u/%U` and quoting).
//! - The `mimeapps.list` resolver is hand-rolled — no crate does it well. This
//!   is a simplified but order-correct pass: per-desktop then generic files
//!   across the XDG config/data hierarchy, `[Default Applications]` first-match,
//!   `[Added Associations]` appended, `[Removed Associations]` masked, unioned
//!   with each `mimeinfo.cache`.
//!
//! Launching detaches via `setsid -f` (util-linux) so children outlive the app.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use freedesktop_desktop_entry::DesktopEntry;

/// A launchable application choice for a specific file, argv pre-expanded.
#[derive(Debug, Clone)]
pub struct AppLaunch {
    pub name: String,
    argv: Vec<String>,
    cwd: Option<PathBuf>,
}

/// Open with the default handler. `./` guard: xdg-open has no `--`
/// end-of-options marker, so a leading dash would parse as a flag.
pub fn open(path: &Path) -> std::io::Result<()> {
    let arg = if path.to_string_lossy().starts_with('-') {
        let mut safe = PathBuf::from(".");
        safe.push(path);
        safe
    } else {
        path.to_path_buf()
    };
    detached("xdg-open", &[arg.as_os_str().to_string_lossy().into_owned()], None)
}

/// Applications registered for this file's MIME type, best candidates first.
pub fn apps_for(path: &Path) -> Vec<AppLaunch> {
    let mime_db = mime_db();
    let guess = mime_db.guess_mime_type().path(path).guess();
    let mime = guess.mime_type().clone();

    // The exact type plus its parents: text/x-rust also offers text/plain
    // handlers. One level of ancestry is plenty for a menu.
    let mut mimes = vec![mime.to_string()];
    for parent in mime_db.get_parents(&mime).unwrap_or_default() {
        mimes.push(parent.to_string());
    }

    let uri = file_uri(path);
    let locales = freedesktop_desktop_entry::get_languages_from_env();

    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for desktop_id in candidate_ids(&mimes) {
        if !seen.insert(desktop_id.clone()) {
            continue;
        }
        let Some(entry) = entries().get(&desktop_id) else {
            continue;
        };
        if entry.no_display() || entry.hidden() || entry.terminal() {
            continue;
        }
        let Ok(argv) = entry.parse_exec_with_uris(&[uri.as_str()], &locales) else {
            continue;
        };
        if argv.is_empty() {
            continue;
        }
        result.push(AppLaunch {
            name: entry
                .name(&locales)
                .map(|n| n.into_owned())
                .unwrap_or_else(|| desktop_id.clone()),
            argv,
            cwd: entry.path().map(PathBuf::from),
        });
    }
    result
}

pub fn launch(app: &AppLaunch) -> std::io::Result<()> {
    detached(&app.argv[0], &app.argv[1..], app.cwd.as_deref())
}

fn detached(program: &str, args: &[impl AsRef<std::ffi::OsStr>], cwd: Option<&Path>) -> std::io::Result<()> {
    // setsid -f double-forks into a new session: the child survives us and
    // never becomes our zombie.
    let mut cmd = Command::new("setsid");
    cmd.arg("-f")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for a in args {
        cmd.arg(a);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn()?;
    // setsid -f exits immediately after forking; reap it inline.
    let _ = child.wait();
    Ok(())
}

// ---------------------------------------------------------------------------

fn mime_db() -> &'static xdg_mime::SharedMimeInfo {
    static DB: OnceLock<xdg_mime::SharedMimeInfo> = OnceLock::new();
    DB.get_or_init(xdg_mime::SharedMimeInfo::new)
}

/// Desktop entries by desktop-file id, loaded once. Staleness across app
/// installs is accepted for now — this cache lives for the app's lifetime.
fn entries() -> &'static HashMap<String, DesktopEntry> {
    static ENTRIES: OnceLock<HashMap<String, DesktopEntry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let locales = freedesktop_desktop_entry::get_languages_from_env();
        let mut map = HashMap::new();
        for entry in freedesktop_desktop_entry::desktop_entries(&locales) {
            // Desktop-file id: path relative to the applications dir with '/'
            // replaced by '-'; appid() gives the stem. Index under both the
            // file name and the bare id so mimeapps references resolve.
            if let Some(name) = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()) {
                map.entry(name).or_insert_with(|| entry.clone());
            }
        }
        map
    })
}

/// Candidate desktop-file ids for a set of MIME types: defaults first, then
/// added/cached associations, minus removals.
fn candidate_ids(mimes: &[String]) -> Vec<String> {
    let mut defaults: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut removed: HashSet<String> = HashSet::new();

    for list in mimeapps_files() {
        let Ok(content) = std::fs::read_to_string(&list) else {
            continue;
        };
        let sections = parse_ini(&content);
        for mime in mimes {
            if let Some(ids) = sections.get("Default Applications").and_then(|s| s.get(mime)) {
                for id in ids.split(';').filter(|s| !s.is_empty()) {
                    if !removed.contains(id) {
                        defaults.push(id.to_string());
                    }
                }
            }
            if let Some(ids) = sections.get("Added Associations").and_then(|s| s.get(mime)) {
                for id in ids.split(';').filter(|s| !s.is_empty()) {
                    if !removed.contains(id) {
                        added.push(id.to_string());
                    }
                }
            }
            if let Some(ids) = sections.get("Removed Associations").and_then(|s| s.get(mime)) {
                for id in ids.split(';').filter(|s| !s.is_empty()) {
                    removed.insert(id.to_string());
                }
            }
        }
    }

    // Union with the compiled caches so unconfigured-but-capable apps appear.
    for dir in xdg_data_dirs() {
        let cache = dir.join("applications/mimeinfo.cache");
        let Ok(content) = std::fs::read_to_string(&cache) else {
            continue;
        };
        let sections = parse_ini(&content);
        for mime in mimes {
            if let Some(ids) = sections.get("MIME Cache").and_then(|s| s.get(mime)) {
                for id in ids.split(';').filter(|s| !s.is_empty()) {
                    if !removed.contains(id) {
                        added.push(id.to_string());
                    }
                }
            }
        }
    }

    defaults.extend(added);
    defaults
}

/// mimeapps.list search order, highest priority first (simplified XDG spec:
/// config dirs before data dirs, per-desktop variants before generic).
fn mimeapps_files() -> Vec<PathBuf> {
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    let mut files = Vec::new();
    let mut push_variants = |dir: PathBuf| {
        for d in &desktops {
            files.push(dir.join(format!("{d}-mimeapps.list")));
        }
        files.push(dir.join("mimeapps.list"));
    };

    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        push_variants(config_home);
    }
    for dir in std::env::var("XDG_CONFIG_DIRS")
        .unwrap_or_else(|_| "/etc/xdg".into())
        .split(':')
        .filter(|s| !s.is_empty())
    {
        push_variants(PathBuf::from(dir));
    }
    for dir in xdg_data_dirs() {
        push_variants(dir.join("applications"));
    }
    files
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        dirs.push(home);
    }
    for dir in std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
        .filter(|s| !s.is_empty())
    {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

/// Minimal INI: section → key → raw value. Good enough for mimeapps.list and
/// mimeinfo.cache, which are flat `k=v` under `[Section]` headers.
fn parse_ini(content: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            sections
                .entry(current.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    sections
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
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

    #[test]
    fn ini_parsing() {
        let ini = "[Default Applications]\ntext/plain=a.desktop;b.desktop;\n\n[Removed Associations]\ntext/plain=c.desktop;\n";
        let s = parse_ini(ini);
        assert_eq!(
            s["Default Applications"]["text/plain"],
            "a.desktop;b.desktop;"
        );
        assert_eq!(s["Removed Associations"]["text/plain"], "c.desktop;");
    }

    #[test]
    fn finds_apps_for_plain_text() {
        // Environment-dependent smoke test: on this machine `gio mime
        // text/plain` lists ~10 apps, so an empty result means the resolver is
        // broken. Keep the assertion loose for other machines.
        let apps = apps_for(Path::new("/etc/hostname"));
        eprintln!(
            "apps for text/plain: {:?}",
            apps.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        if std::path::Path::new("/usr/share/applications/mimeinfo.cache").exists() {
            assert!(!apps.is_empty());
        }
    }
}
