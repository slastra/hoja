//! `file://` URIs: written as GLib writes them, read in any spelling.
//!
//! One spelling, because hoja's URIs go to places that compare them as
//! strings as well as places that decode them. The thumbnail cache names a
//! file by the MD5 of its URI, so a URI that escapes one character more than
//! GLib does names a different file than the one nemo wrote. Drag targets,
//! the clipboard and launched applications decode, and take GLib's spelling
//! because GLib's applications send it.
//!
//! GLib's `g_filename_to_uri` leaves the RFC 3986 unreserved characters, `/`,
//! and `!$&'()*+,:=@` as they are, and escapes every other byte, upper case.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The path of a `file://` URI, with or without an authority (usually empty
/// or `localhost`). `None` for any other scheme or a broken escape.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let encoded = &rest[rest.find('/')?..];

    let mut bytes = Vec::with_capacity(encoded.len());
    let mut iter = encoded.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_as_glib_writes_it() {
        // Parentheses, plus and comma stay as they are; a space and a
        // non-ASCII byte do not. Checked against names nemo wrote.
        assert_eq!(
            file_uri(Path::new("/m/Live (1999), take+2.mp3")),
            "file:///m/Live%20(1999),%20take+2.mp3"
        );
        assert_eq!(file_uri(Path::new("/m/café#1")), "file:///m/caf%C3%A9%231");
        // A literal percent must not survive as one, or the receiver decodes
        // a sequence that was never written.
        assert_eq!(file_uri(Path::new("/tmp/100%")), "file:///tmp/100%25");
    }

    #[test]
    fn round_trip() {
        let path = Path::new("/home/user/with space/naïve (1) & 50%?.txt");
        assert_eq!(uri_to_path(&file_uri(path)).unwrap(), path);
    }

    #[test]
    fn read_in_any_spelling() {
        assert_eq!(
            uri_to_path("file:///etc/hostname").unwrap(),
            Path::new("/etc/hostname")
        );
        assert_eq!(
            uri_to_path("file://localhost/etc/hostname").unwrap(),
            Path::new("/etc/hostname")
        );
        // The stricter spelling other programs write.
        assert_eq!(
            uri_to_path("file:///m/Live%20%281999%29").unwrap(),
            Path::new("/m/Live (1999)")
        );
        assert!(uri_to_path("https://example.com/x").is_none());
        assert!(uri_to_path("file:///broken%2").is_none());
    }
}
