//! Pictures from the freedesktop thumbnail spec.
//!
//! The cache in `$XDG_CACHE_HOME/thumbnails` is shared by every file manager
//! on the desktop, so a picture nemo made is one hoja does not make again, and
//! the other way round. A cached picture is found by name and trusted by
//! content:
//!
//! - **The name** is the MD5 of the file's URI, in lowercase hex. The URI must
//!   be escaped exactly as GLib escapes it, because GLib is what wrote most of
//!   the cache. GLib leaves `!$&'()*+,:=@` alone where a stricter encoder
//!   escapes them, and a song called "Live (1999).mp3" hashes to a different
//!   name under the two. `opener::file_uri` is the stricter one.
//! - **The content** carries `Thumb::URI` and `Thumb::MTime` in PNG text
//!   chunks. A picture whose `Thumb::MTime` is not the file's mtime is of an
//!   older version of the file.
//!
//! A miss runs the thumbnailer that the desktop registered for the file's type
//! (`/usr/share/thumbnailers/*.thumbnailer`), and hoja writes what it makes
//! back into the cache with the text chunks added. A thumbnailer that fails
//! leaves a mark under `fail/hoja-<version>/`, so the same file is not tried
//! on every hover.
//!
//! An image that no installed thumbnailer takes is decoded here instead, with
//! the `image` crate, for the formats gpui compiles it with.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use md5::{Digest as _, Md5};

/// A thumbnailer that runs longer than this is killed and the file is marked
/// as failed. A poster frame from a local video takes well under a second.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The spec's size directories that hoja uses. The tooltip draws a picture at
/// most 256 logical pixels across, so `XLarge` is for a scaled display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Size {
    Large,
    XLarge,
}

impl Size {
    pub fn for_scale(scale: f32) -> Self {
        if scale > 1.0 {
            Size::XLarge
        } else {
            Size::Large
        }
    }

    pub fn pixels(self) -> u32 {
        match self {
            Size::Large => 256,
            Size::XLarge => 512,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Size::Large => "large",
            Size::XLarge => "x-large",
        }
    }
}

/// A decoded thumbnail, in the BGRA order gpui's `RenderImage` takes.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// The original's size in pixels, when the thumbnail says so.
    pub original: Option<(u32, u32)>,
}

/// The cached picture for `path`, made now if the cache has none.
///
/// `mimes` is the file's type followed by its ancestors, nearest first. Blocks
/// for as long as a thumbnailer runs: call it off the UI thread.
pub fn picture(path: &Path, mimes: &[String], size: Size) -> Option<Picture> {
    let root = root()?;
    // The spec: never make thumbnails of thumbnails.
    if path.starts_with(&root) {
        return None;
    }
    let mtime = mtime_of(path)?;
    let uri = glib_uri(path);
    let name = format!("{}.png", md5_hex(&uri));

    let cached = root.join(size.dir()).join(&name);
    if let Ok(bytes) = std::fs::read(&cached)
        && let Some(decoded) = decode(&bytes)
        && decoded.describes(&uri, mtime)
    {
        return Some(decoded.into_picture());
    }

    let failed = root.join("fail").join(fail_dir()).join(&name);
    if let Ok(bytes) = std::fs::read(&failed)
        && decode(&bytes).is_some_and(|mark| mark.describes(&uri, mtime))
    {
        return None;
    }

    let info = Info {
        uri: &uri,
        mtime,
        size: std::fs::metadata(path).ok().map(|m| m.len()),
        mime: mimes.first().map(String::as_str),
    };
    match make(path, mimes, size, &root, &info) {
        Made::Picture(rgba) => {
            // A cache that cannot be written still leaves the picture to show.
            if let Err(err) = store(&cached, &rgba, &info) {
                eprintln!("[hoja] thumbnail not cached: {err}");
            }
            Some(Picture {
                width: rgba.width(),
                height: rgba.height(),
                original: None,
                bgra: bgra_of(rgba.into_raw()),
            })
        }
        Made::Failed => {
            let mark = image::RgbaImage::new(1, 1);
            let _ = store(&failed, &mark, &info);
            None
        }
        // Nothing is registered for this type, which can change when a
        // thumbnailer is installed, so no mark.
        Made::Nothing => None,
    }
}

// ---------------------------------------------------------------------------
// Names

/// `file://` and the path, escaped as GLib's `g_filename_to_uri` escapes it.
pub fn glib_uri(path: &Path) -> String {
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

fn md5_hex(text: &str) -> String {
    Md5::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn root() -> Option<PathBuf> {
    Some(crate::config::cache_home()?.join("thumbnails"))
}

fn fail_dir() -> String {
    format!("hoja-{}", env!("CARGO_PKG_VERSION"))
}

fn mtime_of(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

// ---------------------------------------------------------------------------
// Reading the cache

struct Decoded {
    rgba: image::RgbaImage,
    text: HashMap<String, String>,
}

impl Decoded {
    fn describes(&self, uri: &str, mtime: u64) -> bool {
        self.text.get("Thumb::URI").map(String::as_str) == Some(uri)
            && self
                .text
                .get("Thumb::MTime")
                .and_then(|value| parse_mtime(value))
                == Some(mtime)
    }

    fn into_picture(self) -> Picture {
        let dimension = |key: &str| self.text.get(key)?.trim().parse::<u32>().ok();
        let original = dimension("Thumb::Image::Width").zip(dimension("Thumb::Image::Height"));
        Picture {
            width: self.rgba.width(),
            height: self.rgba.height(),
            original,
            bgra: bgra_of(self.rgba.into_raw()),
        }
    }
}

/// Seconds, as an integer. Some writers have put a fraction on it.
fn parse_mtime(value: &str) -> Option<u64> {
    let value = value.trim();
    value.parse::<u64>().ok().or_else(|| {
        value
            .parse::<f64>()
            .ok()
            .filter(|v| *v >= 0.0)
            .map(|v| v as u64)
    })
}

/// A PNG and its text chunks, including any written after the image data.
fn decode(bytes: &[u8]) -> Option<Decoded> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    // Reads the chunks after the image data, which is where some writers put
    // their text. A failure here leaves what was read before it.
    let _ = reader.finish();

    let pixels = (frame.width as usize) * (frame.height as usize);
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        // `normalize_to_color8` expands a palette, so this does not arrive.
        png::ColorType::Indexed => return None,
    };
    if rgba.len() != pixels * 4 {
        return None;
    }
    let info = reader.info();
    let mut text = HashMap::new();
    for chunk in &info.uncompressed_latin1_text {
        text.insert(chunk.keyword.clone(), chunk.text.clone());
    }
    for chunk in &info.compressed_latin1_text {
        if let Ok(value) = chunk.get_text() {
            text.insert(chunk.keyword.clone(), value);
        }
    }
    for chunk in &info.utf8_text {
        if let Ok(value) = chunk.get_text() {
            text.insert(chunk.keyword.clone(), value);
        }
    }
    Some(Decoded {
        rgba: image::RgbaImage::from_raw(frame.width, frame.height, rgba)?,
        text,
    })
}

/// RGBA to BGRA, in place. Straight alpha either way: gpui's own image
/// decode hands the renderer straight alpha too.
fn bgra_of(mut pixels: Vec<u8>) -> Vec<u8> {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    pixels
}

// ---------------------------------------------------------------------------
// Writing the cache

struct Info<'a> {
    uri: &'a str,
    mtime: u64,
    size: Option<u64>,
    mime: Option<&'a str>,
}

/// Write `rgba` to `dest` with the spec's text chunks, atomically.
fn store(dest: &Path, rgba: &image::RgbaImage, info: &Info) -> std::io::Result<()> {
    let dir = dest.parent().expect("a cache file has a directory");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let other = |err: png::EncodingError| std::io::Error::other(err.to_string());

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, rgba.width(), rgba.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .add_text_chunk("Thumb::URI".into(), info.uri.into())
            .map_err(other)?;
        encoder
            .add_text_chunk("Thumb::MTime".into(), info.mtime.to_string())
            .map_err(other)?;
        if let Some(size) = info.size {
            encoder
                .add_text_chunk("Thumb::Size".into(), size.to_string())
                .map_err(other)?;
        }
        if let Some(mime) = info.mime {
            encoder
                .add_text_chunk("Thumb::Mimetype".into(), mime.into())
                .map_err(other)?;
        }
        encoder
            .add_text_chunk(
                "Software".into(),
                format!("hoja {}", env!("CARGO_PKG_VERSION")),
            )
            .map_err(other)?;
        let mut writer = encoder.write_header().map_err(other)?;
        writer.write_image_data(rgba.as_raw()).map_err(other)?;
    }

    // The spec asks for a temporary name in the same directory and a rename,
    // so no reader ever sees half a file. Mode 0600 because a thumbnail shows
    // what is in a file the user may not want others to see.
    let temp = temp_in(dir);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_data());
    drop(file);
    match written.and_then(|()| std::fs::rename(&temp, dest)) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err)
        }
    }
}

fn temp_in(dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".hoja-{}-{n}.png", std::process::id()))
}

// ---------------------------------------------------------------------------
// Making a picture

enum Made {
    Picture(image::RgbaImage),
    Failed,
    Nothing,
}

fn make(path: &Path, mimes: &[String], size: Size, root: &Path, info: &Info) -> Made {
    if let Some(thumbnailer) = mimes.iter().find_map(|mime| thumbnailers().get(mime)) {
        return run(thumbnailer, path, size, root, info);
    }
    if mimes.first().is_some_and(|mime| mime.starts_with("image/")) {
        return match image::open(path) {
            Ok(decoded) => Made::Picture(fit(decoded, size)),
            Err(_) => Made::Failed,
        };
    }
    Made::Nothing
}

/// Shrunk to fit `size` on its long side. Never enlarged: the spec keeps a
/// small original at its own size.
fn fit(decoded: image::DynamicImage, size: Size) -> image::RgbaImage {
    let max = size.pixels();
    if decoded.width() <= max && decoded.height() <= max {
        decoded.into_rgba8()
    } else {
        decoded
            .resize(max, max, image::imageops::FilterType::Triangle)
            .into_rgba8()
    }
}

fn run(thumbnailer: &Thumbnailer, path: &Path, size: Size, root: &Path, info: &Info) -> Made {
    let dir = root.join(size.dir());
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .is_err()
    {
        return Made::Nothing;
    }
    let out = temp_in(&dir);
    let argv = expand(
        &thumbnailer.exec,
        &Codes {
            uri: info.uri,
            input: &path.to_string_lossy(),
            output: &out.to_string_lossy(),
            size: size.pixels(),
        },
    );
    let ok = argv.split_first().is_some_and(|(program, args)| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok_and(|child| wait(child, TIMEOUT))
    });
    let made = ok
        .then(|| image::open(&out).ok())
        .flatten()
        .map(|decoded| Made::Picture(fit(decoded, size)))
        .unwrap_or(Made::Failed);
    let _ = std::fs::remove_file(&out);
    made
}

/// `true` when the child exited with success inside `limit`. Killed if not.
fn wait(mut child: std::process::Child, limit: Duration) -> bool {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thumbnailers

#[derive(Debug, PartialEq, Eq)]
struct Thumbnailer {
    /// The `Exec` line, split into words, field codes not yet substituted.
    exec: Vec<String>,
}

/// Every installed thumbnailer by the types it takes. Read once: a
/// thumbnailer installed while hoja runs is found at the next start.
fn thumbnailers() -> &'static HashMap<String, std::sync::Arc<Thumbnailer>> {
    static INDEX: OnceLock<HashMap<String, std::sync::Arc<Thumbnailer>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index = HashMap::new();
        // The data dirs in precedence order. A file name seen in an earlier
        // dir hides the one of the same name in a later dir, which is how a
        // user's own file replaces a system one.
        let mut seen_files = HashSet::new();
        for dir in crate::opener::xdg_data_dirs() {
            let Ok(read) = std::fs::read_dir(dir.join("thumbnailers")) else {
                continue;
            };
            let mut files: Vec<_> = read.flatten().map(|e| e.path()).collect();
            files.sort();
            for file in files {
                if file.extension().is_none_or(|ext| ext != "thumbnailer") {
                    continue;
                }
                let Some(name) = file.file_name().map(|n| n.to_os_string()) else {
                    continue;
                };
                if !seen_files.insert(name) {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let Some(entry) = parse(&content) else {
                    continue;
                };
                if !entry.runnable() {
                    continue;
                }
                let thumbnailer = std::sync::Arc::new(Thumbnailer { exec: entry.exec });
                for mime in entry.mimes {
                    index.entry(mime).or_insert_with(|| thumbnailer.clone());
                }
            }
        }
        index
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    exec: Vec<String>,
    mimes: Vec<String>,
    try_exec: Option<String>,
}

impl Entry {
    fn runnable(&self) -> bool {
        let program = self
            .try_exec
            .as_deref()
            .or(self.exec.first().map(String::as_str));
        program.is_some_and(on_path)
    }
}

fn on_path(program: &str) -> bool {
    let program = Path::new(program);
    if program.is_absolute() {
        return program.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// The `[Thumbnailer Entry]` group of a `.thumbnailer` file.
fn parse(content: &str) -> Option<Entry> {
    let mut in_group = false;
    let mut exec = None;
    let mut mimes = Vec::new();
    let mut try_exec = None;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Thumbnailer Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => exec = Some(split_exec(value.trim())),
            "TryExec" => try_exec = Some(value.trim().to_string()),
            "MimeType" => {
                mimes = value
                    .split(';')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    }
    let exec = exec.filter(|words| !words.is_empty())?;
    Some(Entry {
        exec,
        mimes,
        try_exec,
    })
}

/// Words of an `Exec` line, by the desktop entry spec's quoting: a double
/// quoted word may hold spaces, and inside one a backslash escapes the next
/// character.
fn split_exec(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            ch => {
                word.push(ch);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

struct Codes<'a> {
    uri: &'a str,
    input: &'a str,
    output: &'a str,
    size: u32,
}

/// The `Exec` words with the thumbnailer spec's field codes filled in: `%u`
/// the URI, `%i` the path, `%o` the output, `%s` the size, `%%` a percent
/// sign. An unknown code is dropped.
fn expand(words: &[String], codes: &Codes) -> Vec<String> {
    words
        .iter()
        .map(|word| {
            let mut out = String::new();
            let mut chars = word.chars();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    out.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('u') => out.push_str(codes.uri),
                    Some('i') => out.push_str(codes.input),
                    Some('o') => out.push_str(codes.output),
                    Some('s') => out.push_str(&codes.size.to_string()),
                    Some('%') => out.push('%'),
                    _ => {}
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_the_md5_of_the_uri() {
        // The worked example in the thumbnail spec.
        assert_eq!(
            md5_hex("file:///home/jens/photos/me.png"),
            "c6ee772d9e49320e97ec29a7eb5b1697"
        );
    }

    #[test]
    fn the_uri_is_escaped_as_glib_escapes_it() {
        // Parentheses, plus and comma stay as they are; a space and a
        // non-ASCII byte do not. Checked against names nemo wrote.
        assert_eq!(
            glib_uri(Path::new("/m/Live (1999), take+2.mp3")),
            "file:///m/Live%20(1999),%20take+2.mp3"
        );
        assert_eq!(glib_uri(Path::new("/m/café#1")), "file:///m/caf%C3%A9%231");
    }

    #[test]
    fn a_thumbnailer_file_is_read() {
        let entry = parse(
            "[Thumbnailer Entry]\n\
             TryExec=/usr/bin/gst-video-thumbnailer\n\
             Exec=/usr/bin/gst-video-thumbnailer --input-uri %u --output %o --size %s\n\
             MimeType=video/mp4;video/webm;\n\
             [Other]\nExec=nope\n",
        )
        .unwrap();
        assert_eq!(entry.exec[0], "/usr/bin/gst-video-thumbnailer");
        assert_eq!(entry.mimes, ["video/mp4", "video/webm"]);
        assert_eq!(
            entry.try_exec.as_deref(),
            Some("/usr/bin/gst-video-thumbnailer")
        );
    }

    #[test]
    fn a_file_without_exec_is_not_a_thumbnailer() {
        assert_eq!(parse("[Thumbnailer Entry]\nMimeType=a/b;\n"), None);
    }

    #[test]
    fn codes_are_filled_in() {
        let words = split_exec(r#"thumb -s %s "%i" %o 100%% "a \"b\"""#);
        let argv = expand(
            &words,
            &Codes {
                uri: "file:///x%20y",
                input: "/x y",
                output: "/out.png",
                size: 256,
            },
        );
        assert_eq!(
            argv,
            ["thumb", "-s", "256", "/x y", "/out.png", "100%", "a \"b\""]
        );
        // A percent-escape in the URI is not taken for a code.
        assert_eq!(
            expand(
                &["%u".into()],
                &Codes {
                    uri: "file:///a%20b",
                    input: "",
                    output: "",
                    size: 1
                }
            ),
            ["file:///a%20b"]
        );
    }

    fn info<'a>(uri: &'a str, mtime: u64) -> Info<'a> {
        Info {
            uri,
            mtime,
            size: Some(10),
            mime: Some("image/png"),
        }
    }

    #[test]
    fn a_cached_picture_is_trusted_only_for_its_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("large/abc.png");
        let rgba = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        store(&dest, &rgba, &info("file:///a.png", 1700000000)).unwrap();

        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&dest).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        let decoded = decode(&std::fs::read(&dest).unwrap()).unwrap();
        assert!(decoded.describes("file:///a.png", 1700000000));
        assert!(!decoded.describes("file:///a.png", 1700000001));
        assert!(!decoded.describes("file:///b.png", 1700000000));

        let picture = decoded.into_picture();
        assert_eq!((picture.width, picture.height), (3, 2));
        // Blue first: BGRA.
        assert_eq!(&picture.bgra[..4], &[30, 20, 10, 255]);
        // No temporary file left behind.
        assert_eq!(
            std::fs::read_dir(dir.path().join("large")).unwrap().count(),
            1
        );
    }

    #[test]
    fn a_fractional_mtime_is_read() {
        assert_eq!(parse_mtime("1700000000"), Some(1700000000));
        assert_eq!(parse_mtime(" 1700000000.25 "), Some(1700000000));
        assert_eq!(parse_mtime("soon"), None);
    }

    #[test]
    fn a_large_image_is_shrunk_and_a_small_one_kept() {
        let big = image::DynamicImage::new_rgba8(1000, 500);
        let out = fit(big, Size::Large);
        assert_eq!((out.width(), out.height()), (256, 128));
        let small = image::DynamicImage::new_rgba8(40, 30);
        let out = fit(small, Size::XLarge);
        assert_eq!((out.width(), out.height()), (40, 30));
    }
}
