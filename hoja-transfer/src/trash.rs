//! Deletion via the freedesktop.org Trash specification, version 1.0.
//!
//! pane exposes no trash *feature* — no browser, no restore list, no empty
//! command. It uses the trash *directory* purely as the staging area that makes
//! delete undoable, because a file that has been unlinked cannot be brought
//! back. Following the spec instead of inventing a private staging directory
//! costs the same amount of code and means anything pane leaves behind is
//! already understood by `gio trash --empty`, Nautilus, and Dolphin.
//!
//! Every deletion is a `rename` into a trash directory **on the same
//! filesystem** as the file, so it is O(1) and cannot half-finish. If no such
//! directory can be created, the delete fails rather than silently copying
//! gigabytes to the home volume.
//!
//! The details below were verified against `gio trash` on a real system rather
//! than read off the spec, because the spec leaves several of them open:
//!
//! - `Path=` is percent-encoded with the RFC 3986 unreserved set
//!   (`A-Za-z0-9` and `-._~`) left alone, and `/` preserved as a separator.
//!   Note `!*'()` **are** escaped, so this is not RFC 2396's larger "mark" set.
//! - `Path=` is absolute in the home trash and **relative to the volume root**
//!   in a volume trash. Getting this backwards breaks restore in every other
//!   trash UI.
//! - `DeletionDate=` is local time, `YYYY-MM-DDThh:mm:ss`, with no zone suffix.
//! - Name collisions get a counter before the extension — `dup.txt` becomes
//!   `dup.2.txt` — not our ` (copy)` suffix.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::sys::{self, stem_end};

/// A file that has been moved to a trash directory, and everything needed to
/// put it back. Held by the UI's undo stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedItem {
    /// Absolute path the item came from.
    pub original: PathBuf,
    /// `$trash/files/$name`.
    pub file: PathBuf,
    /// `$trash/info/$name.trashinfo`.
    pub info: PathBuf,
}

/// Move `path` into the appropriate trash directory.
///
/// `path` must be absolute. The item keeps its own name where possible, and
/// gains a `.2`, `.3`, … counter when that name is taken.
pub fn trash(path: &Path) -> io::Result<TrashedItem> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("trash needs an absolute path: {}", path.display()),
        ));
    }
    // Confirm the item exists before building any trash directory for it, so a
    // typo cannot create `.Trash-1000` on a volume that had none.
    path.symlink_metadata()?;

    let (dir, relative_to) = trash_dir_for(path)?;
    trash_into(path, &dir, relative_to.as_deref())
}

/// Put a trashed item back where it came from.
///
/// Refuses if something already occupies the original path, and removes the
/// `.trashinfo` sidecar only after the item itself is safely back.
pub fn restore(item: &TrashedItem) -> io::Result<()> {
    if let Some(parent) = item.original.parent()
        && !parent.is_dir()
    {
        std::fs::create_dir_all(parent)?;
    }
    sys::rename_no_replace(&item.file, &item.original)?;
    // A leftover sidecar is cosmetic; a lost file is not. Order matters.
    let _ = std::fs::remove_file(&item.info);
    Ok(())
}

/// The trash directory for `path`, plus the root that `Path=` entries in it are
/// relative to (`None` means absolute, which is the home-trash convention).
fn trash_dir_for(path: &Path) -> io::Result<(PathBuf, Option<PathBuf>)> {
    let home = home_trash()?;
    // Comparing the *device* rather than the path prefix is what makes a
    // bind-mounted or symlinked home behave correctly.
    let item_dev = device_of(path)?;
    if device_of(home.parent().unwrap_or(&home)).ok() == Some(item_dev) {
        return Ok((home, None));
    }

    let top = top_dir(path)?;
    Ok((volume_trash(&top)?, Some(top)))
}

fn home_trash() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "neither XDG_DATA_HOME nor HOME is set")
        })?;
    let trash = base.join("Trash");
    std::fs::create_dir_all(trash.join("files"))?;
    std::fs::create_dir_all(trash.join("info"))?;
    Ok(trash)
}

/// Deepest ancestor of `path` still on the same filesystem — the volume root.
fn top_dir(path: &Path) -> io::Result<PathBuf> {
    let dev = device_of(path)?;
    let mut top = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?
        .to_path_buf();
    while let Some(parent) = top.parent() {
        match device_of(parent) {
            Ok(d) if d == dev => top = parent.to_path_buf(),
            _ => break,
        }
    }
    Ok(top)
}

/// `$topdir/.Trash/$uid` when the admin-provided `.Trash` passes the spec's
/// safety checks, otherwise our own `$topdir/.Trash-$uid`.
///
/// The checks on `.Trash` are not optional: it is a directory any user may be
/// invited into, so without the sticky bit another user could replace entries
/// underneath us, and a symlink there could redirect deletions off the volume
/// entirely — which would also silently reintroduce a cross-filesystem copy.
fn volume_trash(top: &Path) -> io::Result<PathBuf> {
    let uid = rustix::process::getuid().as_raw();

    let shared = top.join(".Trash");
    if let Ok(meta) = shared.symlink_metadata()
        && meta.is_dir()
        && !meta.file_type().is_symlink()
        && meta.mode() & 0o1000 != 0
    {
        let mine = shared.join(uid.to_string());
        if std::fs::create_dir_all(mine.join("files")).is_ok()
            && std::fs::create_dir_all(mine.join("info")).is_ok()
        {
            return Ok(mine);
        }
    }

    let own = top.join(format!(".Trash-{uid}"));
    std::fs::create_dir_all(own.join("files"))?;
    std::fs::create_dir_all(own.join("info"))?;
    Ok(own)
}

/// The testable core: move `path` into an already-resolved trash directory.
///
/// `relative_to` is the volume root for a volume trash, `None` for the home
/// trash. Separated from directory resolution so tests can drive a temp dir
/// without a second filesystem.
fn trash_into(path: &Path, trash_dir: &Path, relative_to: Option<&Path>) -> io::Result<TrashedItem> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();

    let recorded = match relative_to {
        Some(root) => path.strip_prefix(root).unwrap_or(path),
        None => path,
    };
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode(recorded),
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")
    );

    // The sidecar is written first, with O_EXCL, and *is* the lock on the name:
    // two processes trashing files called `notes.txt` at the same moment cannot
    // both win, because only one creation succeeds.
    let (claimed, info_path) = claim(trash_dir, &name, &body)?;

    let dest = trash_dir.join("files").join(&claimed);
    if let Err(err) = sys::rename_no_replace(path, &dest) {
        // Never leave a sidecar describing a file that is still in place.
        let _ = std::fs::remove_file(&info_path);
        return Err(err);
    }

    Ok(TrashedItem {
        original: path.to_path_buf(),
        file: dest,
        info: info_path,
    })
}

/// Claim a free name by creating its `.trashinfo` exclusively.
fn claim(trash_dir: &Path, name: &str, body: &str) -> io::Result<(String, PathBuf)> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let info_dir = trash_dir.join("info");
    for attempt in 1..=10_000u32 {
        let candidate = numbered(name, attempt);
        let path = info_dir.join(format!("{candidate}.trashinfo"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                // A truncated sidecar is an unrestorable file, so clean up
                // rather than leave a half-written claim behind.
                if let Err(err) = file.write_all(body.as_bytes()).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(err);
                }
                return Ok((candidate, path));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free trash name for {name}"),
    ))
}

/// `dup.txt` → `dup.txt`, `dup.2.txt`, `dup.3.txt`, … matching `gio trash`.
fn numbered(name: &str, attempt: u32) -> String {
    if attempt <= 1 {
        return name.to_string();
    }
    let (stem, ext) = name.split_at(stem_end(name));
    format!("{stem}.{attempt}{ext}")
}

/// Percent-encode for the `Path=` field: RFC 3986 unreserved characters and the
/// `/` separator pass through, every other byte becomes `%XX`.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn device_of(path: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.dev())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_gio_does() {
        // Verified against `gio trash` output on a real system.
        assert_eq!(
            encode(Path::new("/home/shaun/weird name #1 %ü.txt")),
            "/home/shaun/weird%20name%20%231%20%25%C3%BC.txt"
        );
        assert_eq!(
            encode(Path::new("/x/punct-_.!~*'()+,;=:@&$[]{}.txt")),
            "/x/punct-_.%21~%2A%27%28%29%2B%2C%3B%3D%3A%40%26%24%5B%5D%7B%7D.txt"
        );
        // Unreserved and the separator survive untouched.
        assert_eq!(encode(Path::new("/a-b_c.d~e/f")), "/a-b_c.d~e/f");
    }

    #[test]
    fn collision_counter_goes_before_the_extension() {
        assert_eq!(numbered("dup.txt", 1), "dup.txt");
        assert_eq!(numbered("dup.txt", 2), "dup.2.txt");
        assert_eq!(numbered("dup.txt", 3), "dup.3.txt");
        // stem_end splits at the first dot, so multi-part extensions stay whole.
        assert_eq!(numbered("archive.tar.gz", 2), "archive.2.tar.gz");
        assert_eq!(numbered("README", 2), "README.2");
        assert_eq!(numbered(".bashrc", 2), ".bashrc.2");
    }
}
