//! What an audio file says about itself: its tags and its stream.

use std::path::Path;
use std::time::Duration;

use lofty::prelude::*;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// kbit/s.
    pub bitrate: Option<u32>,
    /// Hz.
    pub sample_rate: Option<u32>,
    /// The file carries a picture, which is what the thumbnailer shows. Known
    /// before the picture is made, so the tooltip can keep room for it.
    pub has_cover: bool,
}

/// `None` when lofty does not know the format or cannot parse the file.
pub fn read(path: &Path) -> Option<Tags> {
    let file = lofty::read_from_path(path).ok()?;
    let properties = file.properties();
    let duration = properties.duration();
    let mut tags = Tags {
        duration: (!duration.is_zero()).then_some(duration),
        bitrate: properties
            .audio_bitrate()
            .or(properties.overall_bitrate())
            .filter(|&rate| rate > 0),
        sample_rate: properties.sample_rate().filter(|&rate| rate > 0),
        ..Tags::default()
    };
    // The primary tag is the one the format prefers. A file can carry several
    // (ID3v2 and APE on one MP3), and the first is a fair second choice.
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        let clean = |value: Option<std::borrow::Cow<'_, str>>| {
            value
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        tags.title = clean(tag.title());
        tags.artist = clean(tag.artist());
        tags.album = clean(tag.album());
    }
    tags.has_cover = file.tags().iter().any(|tag| !tag.pictures().is_empty());
    Some(tags)
}

/// `3:07`, or `1:02:45` for an hour and longer.
pub fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_a_player() {
        assert_eq!(clock(Duration::from_secs(187)), "3:07");
        assert_eq!(clock(Duration::from_secs(5)), "0:05");
        assert_eq!(clock(Duration::from_secs(3765)), "1:02:45");
    }

    #[test]
    fn not_audio_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.mp3");
        std::fs::write(&path, b"plain words, not a stream").unwrap();
        assert_eq!(read(&path), None);
    }
}
