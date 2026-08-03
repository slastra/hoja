//! What a pane is showing, when that is not always a directory.
//!
//! An archive browsed in a pane looks like a directory and is not one. The
//! rows have names, sizes and dates; they do not have paths, nothing can
//! `stat` them, no watcher can be armed on them, `git status` has nothing to
//! run against, and the trash has nothing to move. Every one of those is a
//! call that takes a `&Path` and would happily accept a made-up one.
//!
//! So the pane holds this instead of a `PathBuf`, and asking for a real path
//! returns an `Option`. That is the whole design: the compiler enumerates the
//! places that need an answer, rather than a plausible-looking path reaching
//! a syscall and coming back with something that reads like a fact.

use std::path::{Path, PathBuf};

/// Where a pane is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A real directory.
    Disk(PathBuf),
    /// A directory inside an archive. `inside` is relative to the archive's
    /// root and is empty at the root itself.
    Archive { archive: PathBuf, inside: PathBuf },
}

impl Location {
    /// The real directory this is, and `None` inside an archive.
    ///
    /// Wanted by everything that reads, watches, or writes a directory:
    /// `read_dir`, the watcher, `git status`, the size walk, new folder, and
    /// the destination of a transfer.
    pub fn disk(&self) -> Option<&Path> {
        match self {
            Location::Disk(dir) => Some(dir),
            Location::Archive { .. } => None,
        }
    }

    /// The real file behind this, which is the directory itself on disk and
    /// the archive file inside one.
    ///
    /// This is what a watcher can be armed on in both cases, and what says
    /// which filesystem a pane's contents come from.
    pub fn anchor(&self) -> &Path {
        match self {
            Location::Disk(dir) => dir,
            Location::Archive { archive, .. } => archive,
        }
    }

    pub fn is_disk(&self) -> bool {
        matches!(self, Location::Disk(_))
    }

    /// The archive root, entered from a real path.
    pub fn in_archive(archive: PathBuf) -> Self {
        Location::Archive {
            archive,
            inside: PathBuf::new(),
        }
    }

    /// One level up, and `None` at the root of the filesystem.
    ///
    /// Going up from the root of an archive leaves the archive and lands in
    /// the directory holding it, so the archive behaves like the folder it is
    /// pretending to be rather than like a place you cannot get out of.
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Disk(dir) => dir.parent().map(|p| Location::Disk(p.to_path_buf())),
            Location::Archive { archive, inside } => match inside.parent() {
                Some(up) => Some(Location::Archive {
                    archive: archive.clone(),
                    inside: up.to_path_buf(),
                }),
                None => archive.parent().map(|p| Location::Disk(p.to_path_buf())),
            },
        }
    }

    /// A child directory of this one, by name.
    pub fn join(&self, name: impl AsRef<Path>) -> Location {
        match self {
            Location::Disk(dir) => Location::Disk(dir.join(name)),
            Location::Archive { archive, inside } => Location::Archive {
                archive: archive.clone(),
                inside: inside.join(name),
            },
        }
    }

    /// The identity of a row in this location, which for an archive is the
    /// archive's own path with the member's path on the end.
    ///
    /// Unique, and it keeps the extension the icon and the Kind column read.
    /// Never a path to anything: see `fs::DirEntry::key`.
    pub fn key(&self) -> PathBuf {
        match self {
            Location::Disk(dir) => dir.clone(),
            // Not `archive.join(inside)`: joining an empty path appends a
            // separator, so the root of an archive would read as
            // `/home/x/pack.zip/` and never compare equal to the file it is.
            Location::Archive { archive, inside } if inside.as_os_str().is_empty() => {
                archive.clone()
            }
            Location::Archive { archive, inside } => archive.join(inside),
        }
    }
}

impl std::fmt::Display for Location {
    /// The one line the address bar shows and the window title carries.
    ///
    /// An archive reads as a path straight through it, because that is what it
    /// looks like and what a person would type to get back to it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.key().display())
    }
}

impl serde::Serialize for Location {
    /// As the string a person reads, which is what the probe carries and what
    /// a test asserts on.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip() -> PathBuf {
        PathBuf::from("/home/x/pack.zip")
    }

    #[test]
    fn only_a_real_directory_has_a_path() {
        assert_eq!(
            Location::Disk(PathBuf::from("/home/x")).disk(),
            Some(Path::new("/home/x"))
        );
        assert_eq!(Location::in_archive(zip()).disk(), None);
    }

    #[test]
    fn an_archive_is_anchored_on_its_file() {
        // What the watcher arms on, and what says which filesystem this is.
        assert_eq!(
            Location::in_archive(zip()).join("ttf").anchor(),
            zip().as_path()
        );
    }

    #[test]
    fn going_up_from_the_root_of_an_archive_leaves_it() {
        let inside = Location::in_archive(zip()).join("ttf");
        assert_eq!(inside.parent(), Some(Location::in_archive(zip())));
        // Out of the archive entirely, and onto the disk.
        assert_eq!(
            inside.parent().and_then(|p| p.parent()),
            Some(Location::Disk(PathBuf::from("/home/x")))
        );
    }

    #[test]
    fn going_up_stops_at_the_root_of_the_filesystem() {
        assert_eq!(Location::Disk(PathBuf::from("/")).parent(), None);
    }

    #[test]
    fn a_location_reads_as_a_path_straight_through_the_archive() {
        let inside = Location::in_archive(zip()).join("ttf").join("Inter");
        assert_eq!(inside.to_string(), "/home/x/pack.zip/ttf/Inter");
        // And the key matches, so a row's key is its location's key with the
        // row's own name on the end.
        assert_eq!(inside.key(), PathBuf::from("/home/x/pack.zip/ttf/Inter"));
    }

    #[test]
    fn the_root_of_an_archive_reads_as_the_file_itself() {
        // `PathBuf::join("")` appends a separator, so the obvious spelling of
        // this gives `/home/x/pack.zip/`, which is not what anyone typed and
        // does not compare equal to the archive it names.
        let root = Location::in_archive(zip());
        assert_eq!(root.to_string(), "/home/x/pack.zip");
        assert_eq!(root.key(), zip());
    }
}
