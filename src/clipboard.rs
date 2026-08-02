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
            let cut = wl_paste("application/x-kde-cutselection")
                .is_some_and(|v| v.trim() == "1");
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

/// Percent-encode a path into a `file://` URI. Encodes everything outside the
/// RFC 3986 unreserved set plus `/`.
fn file_uri(path: &std::path::Path) -> String {
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

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Strip an authority component (usually empty or localhost).
    let path_start = rest.find('/')?;
    let encoded = &rest[path_start..];

    let mut bytes = Vec::with_capacity(encoded.len());
    let mut iter = encoded.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next()?;
            let lo = iter.next()?;
            let hex = [hi, lo];
            let s = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(s, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

use std::os::unix::ffi::OsStringExt;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn uri_round_trip() {
        let path = Path::new("/home/user/with space/naïve (1).txt");
        let uri = file_uri(path);
        assert!(uri.starts_with("file:///home/user/with%20space/"));
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn parses_gnome_blob_shape() {
        // The reader is exercised end-to-end via wl-paste in the app; here just
        // pin the URI decoding against the format's examples.
        assert_eq!(
            uri_to_path("file:///etc/hostname").unwrap(),
            Path::new("/etc/hostname")
        );
        assert_eq!(
            uri_to_path("file://localhost/etc/hostname").unwrap(),
            Path::new("/etc/hostname")
        );
        assert!(uri_to_path("https://example.com/x").is_none());
    }
}
