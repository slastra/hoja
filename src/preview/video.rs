//! A video's size and length, from its container's header.
//!
//! Two readers, for the two container families that hold nearly every video:
//! ISO base media (MP4, MOV, M4V, 3GP) and Matroska (MKV, WebM). Both seek
//! rather than read through. A camera often writes the MP4 index (`moov`)
//! after the media, and a reader that walks there byte by byte reads the whole
//! file to answer a hover. Seeking, it reads a few kilobytes.
//!
//! Anything else answers `None`, and the preview falls back to a 16:9 frame.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Video {
    /// As a player shows it: after rotation, in pixels.
    pub width: u32,
    pub height: u32,
    /// Width over height on screen. Differs from `width / height` for an
    /// anamorphic Matroska file, which says its display size separately.
    pub aspect: f32,
    pub duration: Option<Duration>,
}

pub fn read(path: &Path) -> Option<Video> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut magic = [0; 12];
    file.read_exact(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    if magic[..4] == EBML_HEADER.to_be_bytes() {
        matroska(&mut file, len)
    } else if matches!(
        &magic[4..8],
        b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
    ) {
        mp4(&mut file, len)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// ISO base media

/// The most of `moov` read into memory. An index is kilobytes for a clip and
/// a few megabytes for hours of footage.
const MAX_MOOV: u64 = 64 << 20;
/// Boxes looked at before giving up, at any one level.
const MAX_BOXES: usize = 4096;

fn mp4<R: Read + Seek>(file: &mut R, len: u64) -> Option<Video> {
    let mut at = 0;
    for _ in 0..MAX_BOXES {
        if at >= len {
            return None;
        }
        file.seek(SeekFrom::Start(at)).ok()?;
        let (kind, header, size) = box_header(file, len - at).ok()?;
        if &kind == b"moov" {
            let body = size - header;
            if body > MAX_MOOV {
                return None;
            }
            let mut moov = vec![0; body as usize];
            file.read_exact(&mut moov).ok()?;
            return parse_moov(&moov);
        }
        at += size;
    }
    None
}

/// The type, the header's length and the whole box's length. A size of zero
/// means "to the end", which only the last box may say.
fn box_header<R: Read>(r: &mut R, remaining: u64) -> io::Result<([u8; 4], u64, u64)> {
    let mut head = [0; 8];
    r.read_exact(&mut head)?;
    let kind = [head[4], head[5], head[6], head[7]];
    let (header, size) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        0 => (8, remaining),
        1 => {
            let mut large = [0; 8];
            r.read_exact(&mut large)?;
            (16, u64::from_be_bytes(large))
        }
        size => (8, size as u64),
    };
    if size < header || size > remaining {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok((kind, header, size))
}

/// The children of a box held in memory, as (type, body).
fn children(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let mut cursor = io::Cursor::new(data);
        let (kind, header, size) = box_header(&mut cursor, data.len() as u64).ok()?;
        let body = &data[header as usize..size as usize];
        data = &data[size as usize..];
        Some((kind, body))
    })
    .take(MAX_BOXES)
}

fn parse_moov(moov: &[u8]) -> Option<Video> {
    let mut duration = None;
    let mut video = None;
    for (kind, body) in children(moov) {
        match &kind {
            b"mvhd" => duration = mvhd_duration(body),
            b"trak" if video.is_none() => video = video_track(body),
            _ => {}
        }
    }
    let (width, height) = video?;
    Some(Video {
        width,
        height,
        aspect: width as f32 / height as f32,
        duration,
    })
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn mvhd_duration(body: &[u8]) -> Option<Duration> {
    let (scale, units) = match body.first()? {
        0 => (be32(body, 12)?, be32(body, 16)? as u64),
        1 => (be32(body, 20)?, be64(body, 24)?),
        _ => return None,
    };
    // All ones is the spec's "unknown".
    if scale == 0 || units == u64::MAX || units == u32::MAX as u64 {
        return None;
    }
    Some(Duration::from_secs_f64(units as f64 / scale as f64))
}

/// The size of a track whose handler is `vide`, turned as its matrix turns it.
fn video_track(trak: &[u8]) -> Option<(u32, u32)> {
    let mut size = None;
    let mut is_video = false;
    for (kind, body) in children(trak) {
        match &kind {
            b"tkhd" => size = tkhd_size(body),
            b"mdia" => {
                is_video = children(body)
                    .any(|(kind, hdlr)| &kind == b"hdlr" && hdlr.get(8..12) == Some(b"vide"))
            }
            _ => {}
        }
    }
    size.filter(|_| is_video)
}

fn tkhd_size(body: &[u8]) -> Option<(u32, u32)> {
    // The matrix and then the size close the box, after fields whose width
    // depends on the version.
    let matrix = match body.first()? {
        0 => 40,
        1 => 52,
        _ => return None,
    };
    let fixed = |at| be32(body, at).map(|v| v as i32);
    let (a, d) = (fixed(matrix)?, fixed(matrix + 16)?);
    let width = be32(body, matrix + 36)? >> 16;
    let height = be32(body, matrix + 40)? >> 16;
    if width == 0 || height == 0 {
        return None;
    }
    // A quarter turn either way leaves nothing on the diagonal.
    Some(if a == 0 && d == 0 {
        (height, width)
    } else {
        (width, height)
    })
}

// ---------------------------------------------------------------------------
// Matroska

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_TYPE: u32 = 0x83;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const DISPLAY_WIDTH: u32 = 0x54B0;
const DISPLAY_HEIGHT: u32 = 0x54BA;
const CLUSTER: u32 = 0x1F43_B675;

/// The most of `Info` or `Tracks` read into memory.
const MAX_ELEMENT: u64 = 1 << 20;

/// A variable-length integer: the first byte's leading zeros say how many
/// bytes follow. An ID keeps its marker bit; a size does not.
fn vint<R: Read>(r: &mut R, keep_marker: bool) -> io::Result<(u64, usize)> {
    let mut first = [0];
    r.read_exact(&mut first)?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 8 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut value = if keep_marker {
        first[0] as u64
    } else {
        first[0] as u64 & (0xFF >> len)
    };
    let mut rest = [0; 7];
    r.read_exact(&mut rest[..len - 1])?;
    for &byte in &rest[..len - 1] {
        value = value << 8 | byte as u64;
    }
    Ok((value, len))
}

/// An element's ID, the length of its header, and its body's size, which is
/// `None` when the file says the size is unknown.
fn element<R: Read>(r: &mut R) -> io::Result<(u32, u64, Option<u64>)> {
    let (id, id_len) = vint(r, true)?;
    let (size, size_len) = vint(r, false)?;
    let unknown = size == (1 << (7 * size_len)) - 1;
    Ok((
        id as u32,
        (id_len + size_len) as u64,
        (!unknown).then_some(size),
    ))
}

/// The children of an element held in memory, as (ID, body).
fn ebml_children(mut data: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let mut cursor = io::Cursor::new(data);
        let (id, header, size) = element(&mut cursor).ok()?;
        let end = header.checked_add(size?)?;
        if end > data.len() as u64 {
            return None;
        }
        let body = &data[header as usize..end as usize];
        data = &data[end as usize..];
        Some((id, body))
    })
    .take(MAX_BOXES)
}

fn uint(body: &[u8]) -> Option<u64> {
    (body.len() <= 8).then(|| body.iter().fold(0, |v, &b| v << 8 | b as u64))
}

fn float(body: &[u8]) -> Option<f64> {
    match body.len() {
        4 => Some(f32::from_be_bytes(body.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(body.try_into().ok()?)),
        _ => None,
    }
}

fn matroska<R: Read + Seek>(file: &mut R, len: u64) -> Option<Video> {
    // The EBML header, skipped.
    let (id, header, size) = element(file).ok()?;
    if id != EBML_HEADER {
        return None;
    }
    let at = header + size?;
    file.seek(SeekFrom::Start(at)).ok()?;
    let (id, header, _) = element(file).ok()?;
    if id != SEGMENT {
        return None;
    }
    // Seek positions count from here.
    let segment = at + header;

    let mut info = None;
    let mut tracks = None;
    let mut seek_tracks = None;
    let mut at = segment;
    for _ in 0..MAX_BOXES {
        if at >= len || (info.is_some() && tracks.is_some()) {
            break;
        }
        file.seek(SeekFrom::Start(at)).ok()?;
        let Ok((id, header, size)) = element(file) else {
            break;
        };
        // The media starts here. What has not been seen yet is wherever the
        // seek head said it was, if it said.
        if id == CLUSTER {
            break;
        }
        let Some(size) = size else {
            break;
        };
        match id {
            INFO | TRACKS | SEEK_HEAD if size <= MAX_ELEMENT => {
                let mut body = vec![0; size as usize];
                file.read_exact(&mut body).ok()?;
                match id {
                    INFO => info = Some(body),
                    TRACKS => tracks = Some(body),
                    _ => seek_tracks = seek_head_tracks(&body).map(|pos| segment + pos),
                }
            }
            _ => {}
        }
        at += header + size;
    }
    if tracks.is_none()
        && let Some(pos) = seek_tracks
    {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let (id, _, size) = element(file).ok()?;
        let size = size.filter(|&s| id == TRACKS && s <= MAX_ELEMENT)?;
        let mut body = vec![0; size as usize];
        file.read_exact(&mut body).ok()?;
        tracks = Some(body);
    }

    let (width, height, aspect) = mkv_video(&tracks?)?;
    Some(Video {
        width,
        height,
        aspect,
        duration: info.as_deref().and_then(mkv_duration),
    })
}

/// Where the seek head says `Tracks` is, from the start of the segment's body.
fn seek_head_tracks(body: &[u8]) -> Option<u64> {
    ebml_children(body)
        .filter(|(id, _)| *id == SEEK)
        .find_map(|(_, seek)| {
            let mut target = None;
            let mut position = None;
            for (id, value) in ebml_children(seek) {
                match id {
                    SEEK_ID => target = uint(value),
                    SEEK_POSITION => position = uint(value),
                    _ => {}
                }
            }
            (target == Some(TRACKS as u64))
                .then_some(position)
                .flatten()
        })
}

fn mkv_duration(info: &[u8]) -> Option<Duration> {
    let mut scale = 1_000_000u64;
    let mut duration = None;
    for (id, body) in ebml_children(info) {
        match id {
            TIMESTAMP_SCALE => scale = uint(body).filter(|&s| s > 0)?,
            DURATION => duration = float(body),
            _ => {}
        }
    }
    let nanos = duration.filter(|d| d.is_finite() && *d > 0.0)? * scale as f64;
    Some(Duration::from_secs_f64(nanos / 1e9))
}

/// The first video track's size in pixels, and its aspect on screen.
fn mkv_video(tracks: &[u8]) -> Option<(u32, u32, f32)> {
    ebml_children(tracks)
        .filter(|(id, _)| *id == TRACK_ENTRY)
        .find_map(|(_, entry)| {
            let mut is_video = false;
            let mut video = None;
            for (id, body) in ebml_children(entry) {
                match id {
                    TRACK_TYPE => is_video = uint(body) == Some(1),
                    VIDEO => video = Some(body),
                    _ => {}
                }
            }
            let video = video.filter(|_| is_video)?;
            let mut size = [None; 4];
            for (id, body) in ebml_children(video) {
                let slot = match id {
                    PIXEL_WIDTH => 0,
                    PIXEL_HEIGHT => 1,
                    DISPLAY_WIDTH => 2,
                    DISPLAY_HEIGHT => 3,
                    _ => continue,
                };
                size[slot] = uint(body).filter(|&v| v > 0 && v <= u32::MAX as u64);
            }
            let (width, height) = (size[0]? as u32, size[1]? as u32);
            // The display size defaults to the pixel size, and only its ratio
            // matters: a file may give it in other units than pixels.
            let (dw, dh) = (
                size[2].unwrap_or(width as u64),
                size[3].unwrap_or(height as u64),
            );
            Some((width, height, dw as f32 / dh as f32))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn tkhd(width: u32, height: u32, quarter_turn: bool) -> Vec<u8> {
        let mut body = vec![0; 40];
        let one = 0x0001_0000u32;
        let matrix: [u32; 9] = if quarter_turn {
            [0, one, 0, one.wrapping_neg(), 0, 0, 0, 0, 0x4000_0000]
        } else {
            [one, 0, 0, 0, one, 0, 0, 0, 0x4000_0000]
        };
        for v in matrix {
            body.extend_from_slice(&v.to_be_bytes());
        }
        body.extend_from_slice(&(width << 16).to_be_bytes());
        body.extend_from_slice(&(height << 16).to_be_bytes());
        mp4_box(b"tkhd", &body)
    }

    fn trak(handler: &[u8; 4], width: u32, height: u32, turn: bool) -> Vec<u8> {
        let mut hdlr = vec![0; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 12]);
        let mdia = mp4_box(b"mdia", &mp4_box(b"hdlr", &hdlr));
        mp4_box(b"trak", &[tkhd(width, height, turn), mdia].concat())
    }

    fn mvhd(scale: u32, units: u32) -> Vec<u8> {
        let mut body = vec![0; 12];
        body.extend_from_slice(&scale.to_be_bytes());
        body.extend_from_slice(&units.to_be_bytes());
        body.extend_from_slice(&[0; 80]);
        mp4_box(b"mvhd", &body)
    }

    fn mp4_file(turn: bool) -> Vec<u8> {
        // The index after the media, as a camera writes it, and an audio
        // track before the video one.
        let moov = mp4_box(
            b"moov",
            &[
                mvhd(1000, 83_500),
                trak(b"soun", 0, 0, false),
                trak(b"vide", 1920, 1080, turn),
            ]
            .concat(),
        );
        [
            mp4_box(b"ftyp", b"isom\0\0\0\0isom"),
            mp4_box(b"mdat", &vec![0xAB; 100_000]),
            moov,
        ]
        .concat()
    }

    fn read_bytes(bytes: &[u8]) -> Option<Video> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        std::fs::write(&path, bytes).unwrap();
        read(&path)
    }

    #[test]
    fn an_mp4_index_after_the_media_is_found() {
        let video = read_bytes(&mp4_file(false)).unwrap();
        assert_eq!((video.width, video.height), (1920, 1080));
        assert_eq!(video.duration, Some(Duration::from_millis(83_500)));
        assert!((video.aspect - 16. / 9.).abs() < 1e-4);
    }

    #[test]
    fn a_turned_phone_video_is_tall() {
        let video = read_bytes(&mp4_file(true)).unwrap();
        assert_eq!((video.width, video.height), (1080, 1920));
    }

    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|&b| b == 0)
            .collect();
        // An eight-byte size, which every reader must take.
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    fn mkv_file(display: Option<(u16, u16)>, tracks_last: bool) -> Vec<u8> {
        let mut video = [
            el(PIXEL_WIDTH, &720u16.to_be_bytes()),
            el(PIXEL_HEIGHT, &576u16.to_be_bytes()),
        ]
        .concat();
        if let Some((w, h)) = display {
            video.extend(el(DISPLAY_WIDTH, &w.to_be_bytes()));
            video.extend(el(DISPLAY_HEIGHT, &h.to_be_bytes()));
        }
        let audio = el(TRACK_ENTRY, &el(TRACK_TYPE, &[2]));
        let entry = el(
            TRACK_ENTRY,
            &[el(TRACK_TYPE, &[1]), el(VIDEO, &video)].concat(),
        );
        let tracks = el(TRACKS, &[audio, entry].concat());
        let info = el(
            INFO,
            &[
                el(TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40]),
                el(DURATION, &12_000.0f64.to_be_bytes()),
            ]
            .concat(),
        );
        let cluster = el(CLUSTER, &[0; 64]);
        let body = if tracks_last {
            // Tracks after the media, found only through the seek head.
            let head_len = el(
                SEEK_HEAD,
                &el(
                    SEEK,
                    &[
                        el(SEEK_ID, &TRACKS.to_be_bytes()),
                        el(SEEK_POSITION, &0u64.to_be_bytes()),
                    ]
                    .concat(),
                ),
            )
            .len();
            let pos = (head_len + info.len() + cluster.len()) as u64;
            let head = el(
                SEEK_HEAD,
                &el(
                    SEEK,
                    &[
                        el(SEEK_ID, &TRACKS.to_be_bytes()),
                        el(SEEK_POSITION, &pos.to_be_bytes()),
                    ]
                    .concat(),
                ),
            );
            assert_eq!(head.len(), head_len);
            [head, info, cluster, tracks].concat()
        } else {
            [info, tracks, cluster].concat()
        };
        [el(EBML_HEADER, &el(0x4282, b"webm")), el(SEGMENT, &body)].concat()
    }

    #[test]
    fn a_matroska_header_is_read() {
        let video = read_bytes(&mkv_file(None, false)).unwrap();
        assert_eq!((video.width, video.height), (720, 576));
        assert_eq!(video.duration, Some(Duration::from_secs(12)));
        assert!((video.aspect - 1.25).abs() < 1e-4);
    }

    #[test]
    fn an_anamorphic_file_takes_its_display_aspect() {
        let video = read_bytes(&mkv_file(Some((16, 9)), false)).unwrap();
        assert_eq!((video.width, video.height), (720, 576));
        assert!((video.aspect - 16. / 9.).abs() < 1e-4);
    }

    #[test]
    fn tracks_after_the_media_are_found_through_the_seek_head() {
        let video = read_bytes(&mkv_file(None, true)).unwrap();
        assert_eq!((video.width, video.height), (720, 576));
    }

    #[test]
    fn other_files_are_none() {
        assert_eq!(read_bytes(b"RIFF\0\0\0\0AVI LIST and so on"), None);
        assert_eq!(read_bytes(b"short"), None);
        // A box that claims more than the file holds.
        let mut bad = mp4_box(b"ftyp", b"isom");
        bad.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xF0, b'm', b'o', b'o', b'v']);
        assert_eq!(read_bytes(&bad), None);
    }
}
