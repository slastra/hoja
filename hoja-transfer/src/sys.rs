//! Platform probes: mount identity, removable-media detection, temp/keep-both names.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{AtFlags, StatxFlags};

/// Identity of the filesystem a path lives on, used as the key of the
/// attempt-failure cache. Prefer the statx mount ID (distinguishes bind mounts);
/// fall back to `st_dev` where the kernel doesn't report one. The two are never
/// compared against each other, the enum keeps them in separate namespaces.
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
/// anywhere in the device's sysfs ancestry, the same walk util-linux does for
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

/// Longest single path component the filesystems we write to will accept: 255
/// bytes on ext4, btrfs and xfs, and 255 UTF-16 code units on exFAT and NTFS.
/// A UTF-8 string never encodes to more UTF-16 units than it has bytes, so a
/// byte budget satisfies both.
const NAME_MAX: usize = 255;

const PARTIAL_PREFIX: &str = ".hoja-partial-";

/// In-progress destination name: hidden, uniquified, same directory as the final
/// destination so the finishing `rename` cannot cross a filesystem.
///
/// The wrapping costs about 26 bytes, which used to be enough to push a name
/// that fitted the filesystem past `NAME_MAX`, so a file `cp` copies without
/// complaint failed here with ENAMETOOLONG, against a path the user never named.
/// The source name is in here only to make a partial file legible while it is
/// being written; the prefix and the uniquifier are what it needs to be correct.
/// So the name is what gives way.
pub fn partial_path(final_dest: &Path) -> PathBuf {
    let name = final_dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let nonce = PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Cached: glibc dropped its getpid cache in 2.25, so `std::process::id()` is
    // a real syscall, and this runs for every file copied.
    static PID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let pid = PID.get_or_init(std::process::id);
    let tail = format!(".{pid}-{nonce}");
    // Truncating cannot collide two partials: the nonce alone is unique.
    let budget = NAME_MAX.saturating_sub(PARTIAL_PREFIX.len() + tail.len());
    let name = truncate_on_char_boundary(&name, budget);
    final_dest.with_file_name(format!("{PARTIAL_PREFIX}{name}{tail}"))
}

/// Longest prefix of `s` within `budget` bytes that does not split a character.
fn truncate_on_char_boundary(s: &str, budget: usize) -> &str {
    if s.len() <= budget {
        return s;
    }
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Listing-side filter for the app: partial files should not appear in panes.
pub fn is_partial_name(name: &str) -> bool {
    name.starts_with(PARTIAL_PREFIX)
}

/// Byte index where a file name's extension begins, or `name.len()` when it
/// has none.
///
/// The split is at the *first* dot, so `foo.tar.gz` stems to `foo`. Leading
/// dots belong to the stem, so `.bashrc` stems to the whole name. Both the
/// rename pre-selection and the ` (copy)` insertion point derive from this,
/// they must agree, so there is one definition.
pub fn stem_end(name: &str) -> usize {
    let leading_dots = name.len() - name.trim_start_matches('.').len();
    match name[leading_dots..].find('.') {
        Some(ix) => leading_dots + ix,
        None => name.len(),
    }
}

/// `foo.tar.gz` → `foo (copy).tar.gz`, then `foo (copy 2).tar.gz`, …
///
/// Budgeted against `NAME_MAX` like `partial_path`, and against a worse failure
/// than that one had. `resolve_dest` picks a candidate by asking whether
/// anything is already there, and a name too long for the filesystem answers
/// that question with ENAMETOOLONG, which reads as "nothing is there". The
/// candidate is accepted, `partial_path` shortens its own name enough to open,
/// the whole file copies, and only the closing rename onto the real name fails.
/// So the cost of getting this wrong is paid after the bytes are written.
///
/// The suffix is what makes this a keep-both and the extension decides what the
/// file is, so the stem is what gives way.
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
    let budget = NAME_MAX.saturating_sub(suffix.len() + ext.len());
    let stem = truncate_on_char_boundary(stem, budget);
    dest.with_file_name(format!("{stem}{suffix}{ext}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_partial_name_stays_within_the_filesystem_limit() {
        // The name that fits is the whole point: the source name here is legal
        // everywhere, and before the budget the partial built from it was not.
        let dest = std::path::PathBuf::from("/dest").join("n".repeat(255));
        let partial = super::partial_path(&dest);
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() <= super::NAME_MAX,
            "{} bytes is over NAME_MAX",
            name.len()
        );
        assert!(super::is_partial_name(name));
    }

    #[test]
    fn a_keep_both_name_stays_within_the_filesystem_limit() {
        // A source name at the limit: the suffix alone pushes it over, which is
        // the case that copied the whole file and then failed at the rename.
        let dest = std::path::PathBuf::from("/dest").join(format!("{}.tar.gz", "n".repeat(248)));
        for attempt in [1, 2, 999] {
            let built = super::keep_both_name(&dest, attempt);
            let name = built.file_name().unwrap().to_str().unwrap();
            assert!(
                name.len() <= super::NAME_MAX,
                "attempt {attempt}: {} bytes is over NAME_MAX",
                name.len()
            );
            assert!(name.ends_with(".tar.gz"), "the extension survives: {name}");
            assert!(name.contains("(copy"), "the suffix survives: {name}");
        }
    }

    #[test]
    fn each_keep_both_attempt_is_a_different_name() {
        // Truncation eats the tail of the stem, which is what would otherwise
        // distinguish them, so the suffix has to keep the attempts apart alone.
        let dest = std::path::PathBuf::from("/dest").join("n".repeat(255));
        let names: std::collections::HashSet<_> =
            (1..=8).map(|a| super::keep_both_name(&dest, a)).collect();
        assert_eq!(names.len(), 8);
    }

    #[test]
    fn a_short_keep_both_name_is_left_whole() {
        let built = super::keep_both_name(std::path::Path::new("/dest/foo.tar.gz"), 1);
        assert_eq!(built.file_name().unwrap(), "foo (copy).tar.gz");
    }

    #[test]
    fn a_short_name_is_left_whole() {
        let partial = super::partial_path(std::path::Path::new("/dest/report.pdf"));
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert!(name.contains("report.pdf"), "{name}");
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        // Every char is 3 bytes, so a budget landing mid-character has to step
        // back: cutting one in half would make the name invalid UTF-8 and
        // `to_str` below would fail.
        let dest = std::path::PathBuf::from("/dest").join("日".repeat(120));
        let partial = super::partial_path(&dest);
        let name = partial.file_name().unwrap().to_str().unwrap();
        assert!(name.len() <= super::NAME_MAX);
    }

    #[test]
    fn two_partials_for_one_destination_never_collide() {
        // Truncation throws away the distinguishing tail of a long name; the
        // nonce is what has to carry uniqueness after that.
        let dest = std::path::PathBuf::from("/dest").join("n".repeat(255));
        assert_ne!(super::partial_path(&dest), super::partial_path(&dest));
    }

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
        assert!(!is_removable_with_sysfs(
            Path::new("/tmp"),
            Path::new("/sys")
        ));
    }
}
