//! Platform probes: mount identity, removable-media detection, temp/keep-both names.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{AtFlags, StatxFlags};

/// Identity of the filesystem a path lives on, used as the key of the
/// attempt-failure cache. Prefer the statx mount ID (distinguishes bind mounts);
/// fall back to `st_dev` where the kernel doesn't report one. The two are never
/// compared against each other — the enum keeps them in separate namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountKey {
    MountId(u64),
    Dev(u64),
}

pub fn mount_key(path: &Path) -> std::io::Result<MountKey> {
    let stx = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::MNT_ID,
    )?;
    // The kernel may ignore the request; only trust fields present in stx_mask.
    if stx.stx_mask & StatxFlags::MNT_ID.bits() != 0 {
        Ok(MountKey::MountId(stx.stx_mnt_id))
    } else {
        Ok(MountKey::Dev(
            u64::from(stx.stx_dev_major) << 32 | u64::from(stx.stx_dev_minor),
        ))
    }
}

/// Whether two paths live on the same filesystem.
///
/// Drives the drag-and-drop default: within one filesystem a move is a rename
/// and effectively free, across filesystems it is a full copy plus a delete, so
/// the convention every file manager follows is to move within and copy across.
///
/// Unknowable answers are `false`. Treating "I could not tell" as "different
/// filesystem" degrades to a copy, which leaves the source in place; the
/// opposite mistake deletes it.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (mount_key(a), mount_key(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether the filesystem holding `path` is on hot-unpluggable media.
///
/// `/sys/block/<dev>/removable` alone is wrong: it reflects the SCSI RMB bit
/// ("medium ejectable from the drive", floppy semantics), and USB enclosures
/// commonly report 0. The reliable signal is a hotplug bus (usb/mmc/firewire)
/// anywhere in the device's sysfs ancestry — the same walk util-linux does for
/// its HOTPLUG column. Mount location under /run/media|/media|/mnt is OR'd in as
/// a heuristic. Bias is deliberately toward false positives: a wasted syncfs is
/// free, a false negative loses data on yank.
pub fn is_removable(path: &Path) -> bool {
    is_removable_with_sysfs(path, Path::new("/sys"))
}

/// Testable core: `sysfs` is injectable so tests can use a fixture tree.
pub fn is_removable_with_sysfs(path: &Path, sysfs: &Path) -> bool {
    if mounted_under_media(path) {
        return true;
    }

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let dev = std::os::unix::fs::MetadataExt::dev(&meta);
    let (major, minor) = (dev >> 8 & 0xfff, dev & 0xff | (dev >> 12 & !0xff));
    if major == 0 {
        return false; // virtual fs: tmpfs, overlay, fuse
    }

    let link = sysfs.join(format!("dev/block/{major}:{minor}"));
    let Ok(resolved) = std::fs::canonicalize(&link) else {
        return false;
    };
    let resolved = resolved.to_string_lossy();

    // Hotplug bus anywhere in the ancestry.
    if resolved.contains("/usb")
        || resolved.contains("/mmc_host/")
        || resolved.contains("/firewire")
        || resolved.contains("/memstick")
    {
        return true;
    }

    // Classic removable flag still catches SD readers and optical on SATA/PCI.
    // The device path looks like .../block/sda/sda1; the flag lives on the disk.
    let mut cursor = PathBuf::from(resolved.as_ref());
    while let Some(parent) = cursor.parent().map(Path::to_path_buf) {
        let flag = cursor.join("removable");
        if let Ok(contents) = std::fs::read_to_string(&flag) {
            return contents.trim() == "1";
        }
        if !parent.to_string_lossy().contains("/block") {
            break;
        }
        cursor = parent;
    }
    false
}

fn mounted_under_media(path: &Path) -> bool {
    let p = path.to_string_lossy();
    p.starts_with("/run/media/") || p.starts_with("/media/") || p.starts_with("/mnt/")
}

/// Rename that refuses to clobber an existing destination.
///
/// Uses `renameat2(RENAME_NOREPLACE)`. Filesystems that do not support the
/// flag return `EINVAL`; those fall back to an existence check plus a plain
/// rename (a small race, but the best available there).
pub fn rename_no_replace(old: &Path, new: &Path) -> std::io::Result<()> {
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        old,
        rustix::fs::CWD,
        new,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::INVAL) => {
            if new.symlink_metadata().is_ok() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(old, new)
        }
        Err(err) => Err(err.into()),
    }
}

static PARTIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// In-progress destination name: hidden, uniquified, same directory as the final
/// destination so the finishing `rename` cannot cross a filesystem.
pub fn partial_path(final_dest: &Path) -> PathBuf {
    let name = final_dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let nonce = PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let unique = format!(
        ".pane-partial-{name}.{}-{nonce}",
        std::process::id(),
    );
    final_dest.with_file_name(unique)
}

/// Listing-side filter for the app: partial files should not appear in panes.
pub fn is_partial_name(name: &str) -> bool {
    name.starts_with(".pane-partial-")
}

/// Byte index where a file name's extension begins, or `name.len()` when it
/// has none.
///
/// The split is at the *first* dot, so `foo.tar.gz` stems to `foo`. Leading
/// dots belong to the stem, so `.bashrc` stems to the whole name. Both the
/// rename pre-selection and the ` (copy)` insertion point derive from this —
/// they must agree, so there is one definition.
pub fn stem_end(name: &str) -> usize {
    let leading_dots = name.len() - name.trim_start_matches('.').len();
    match name[leading_dots..].find('.') {
        Some(ix) => leading_dots + ix,
        None => name.len(),
    }
}

/// `foo.tar.gz` → `foo (copy).tar.gz`, then `foo (copy 2).tar.gz`, …
pub fn keep_both_name(dest: &Path, attempt: u32) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());

    let (stem, ext) = name.split_at(stem_end(&name));

    let suffix = if attempt <= 1 {
        " (copy)".to_string()
    } else {
        format!(" (copy {attempt})")
    };
    dest.with_file_name(format!("{stem}{suffix}{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_both_names() {
        let p = Path::new("/x/foo.tar.gz");
        assert_eq!(keep_both_name(p, 1), Path::new("/x/foo (copy).tar.gz"));
        assert_eq!(keep_both_name(p, 2), Path::new("/x/foo (copy 2).tar.gz"));
        assert_eq!(
            keep_both_name(Path::new("/x/README"), 1),
            Path::new("/x/README (copy)")
        );
        assert_eq!(
            keep_both_name(Path::new("/x/.bashrc"), 1),
            Path::new("/x/.bashrc (copy)")
        );
    }

    #[test]
    fn partial_names_are_hidden_and_unique() {
        let a = partial_path(Path::new("/x/file.txt"));
        let b = partial_path(Path::new("/x/file.txt"));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_partial_name(&name));
        assert_eq!(a.parent(), Some(Path::new("/x")));
    }

    #[test]
    fn removable_media_heuristics() {
        assert!(is_removable_with_sysfs(
            Path::new("/run/media/user/STICK"),
            Path::new("/nonexistent")
        ));
        // tmpfs (/tmp): virtual major 0 → not removable.
        assert!(!is_removable_with_sysfs(Path::new("/tmp"), Path::new("/sys")));
    }
}
