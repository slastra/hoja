//! File clipboard: internal state plus a best-effort Wayland bridge.
//!
//! gpui's clipboard has no custom-MIME API on Linux (the Wayland send callback
//! ignores the requested type entirely), so interop goes through wl-clipboard.
//! One real limitation found during implementation: `wl-copy` offers a single
//! MIME type per invocation and each invocation replaces the selection, so we
//! cannot offer all four desktop flavours at once. The compromise:
//!
//! - clipboard selection ← `x-special/gnome-copied-files` (Nautilus & friends)
//! - primary selection  ← plain newline-separated paths (middle-click paste
//!   into terminals)
//!
//! Reading is full-fidelity: GNOME blob first, then `text/uri-list`, and the
//! cut/copy flag is honored from whichever flavour supplies it. The internal
//! `ClipboardSet` remains the source of truth for in-app paste; the system
//! clipboard is only consulted when another app owns it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::uri::{file_uri, uri_to_path};

#[derive(Debug, Clone)]
pub struct ClipboardSet {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// Mirror a copy/cut to the system clipboard. Spawned fire-and-forget; wl-copy
/// forks itself to serve paste requests, so no child is awaited.
pub fn mirror_to_system(set: &ClipboardSet) {
    let mut blob = String::from(if set.cut { "cut" } else { "copy" });
    for path in &set.paths {
        blob.push('\n');
        blob.push_str(&file_uri(path));
    }
    spawn_wl_copy(&["-t", "x-special/gnome-copied-files"], blob);

    let plain = set
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    spawn_wl_copy(&["--primary"], plain);
}

fn spawn_wl_copy(args: &[&str], payload: String) {
    let spawned = Command::new("wl-copy")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(payload.as_bytes());
        }
        // wl-copy double-forks to serve the selection; reap the intermediate
        // child so it doesn't linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Read another app's file clipboard, if one is offered.
///
/// Blocking (runs wl-paste); call from the background executor, not the UI
/// thread, a slow clipboard owner stalls the read.
pub fn read_external() -> Option<ClipboardSet> {
    if let Some(blob) = wl_paste("x-special/gnome-copied-files") {
        let mut lines = blob.lines();
        let cut = matches!(lines.next(), Some("cut"));
        let paths: Vec<PathBuf> = lines.filter_map(uri_to_path).collect();
        if !paths.is_empty() {
            return Some(ClipboardSet { paths, cut });
        }
    }

    if let Some(list) = wl_paste("text/uri-list") {
        // RFC 2483: CRLF separators, `#` comments.
        let paths: Vec<PathBuf> = list
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(uri_to_path)
            .collect();
        if !paths.is_empty() {
            // KDE marks cut state in a separate flavour: single byte, 1 = cut.
            let cut = wl_paste("application/x-kde-cutselection").is_some_and(|v| v.trim() == "1");
            return Some(ClipboardSet { paths, cut });
        }
    }

    None
}

fn wl_paste(mime: &str) -> Option<String> {
    let output = Command::new("wl-paste")
        .args(["-t", mime, "-n"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
