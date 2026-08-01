//! Metadata preservation: mode, xattrs, mtime. Fail-soft where the destination
//! filesystem cannot hold what the source had (FAT has no mode or xattrs) —
//! those degrade to warnings, never failures.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, Timespec, Timestamps, UTIME_OMIT, XattrFlags};

/// Result of a fail-soft pass: what was lost, if anything.
pub struct MetaOutcome {
    pub warnings: Vec<String>,
}

/// Apply source metadata to an open destination file.
///
/// Ordering matters: mtime goes last, after every write and every other change,
/// or the timestamp gets clobbered by our own subsequent operations.
pub fn apply_file_meta(src: &File, src_meta: &std::fs::Metadata, dest: &File) -> MetaOutcome {
    let mut warnings = Vec::new();

    // Permission bits (setuid/setgid/sticky included; file-type bits masked off).
    let mode = Mode::from_bits_truncate(src_meta.mode() & 0o7777);
    if let Err(err) = rustix::fs::fchmod(dest, mode) {
        warnings.push(format!("mode not preserved: {err}"));
    }

    // Ownership: only meaningful when running with the privilege to change it;
    // EPERM for a regular user copying someone else's file is the normal case.
    let _ = rustix::fs::fchown(
        dest,
        Some(rustix::fs::Uid::from_raw(src_meta.uid())),
        Some(rustix::fs::Gid::from_raw(src_meta.gid())),
    );

    if let Err(detail) = copy_xattrs(src, dest) {
        warnings.push(detail);
    }

    apply_mtime(src_meta, dest, &mut warnings);

    MetaOutcome { warnings }
}

fn apply_mtime(src_meta: &std::fs::Metadata, dest: &File, warnings: &mut Vec<String>) {
    let times = Timestamps {
        // Leave atime alone; copying should not pretend the file was read then.
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: Timespec {
            tv_sec: src_meta.mtime(),
            tv_nsec: src_meta.mtime_nsec(),
        },
    };
    if let Err(err) = rustix::fs::futimens(dest, &times) {
        warnings.push(format!("mtime not preserved: {err}"));
    }
}

/// Copy xattrs between two open files. TOCTOU-free: operates on fds, never paths.
///
/// Errors collapse into a single warning string — one unreadable or unwritable
/// attribute must not fail the copy. `ENOTSUP` from the destination (FAT) is the
/// expected fail-soft case.
pub fn copy_xattrs(src: &File, dest: &File) -> Result<(), String> {
    // 64KB covers every real-world xattr list; values on ext4 fit in a block.
    let mut names = vec![0u8; 64 * 1024];
    let len = match rustix::fs::flistxattr(src, &mut *names) {
        Ok(len) => len,
        // Source fs has no xattrs: nothing to lose.
        Err(rustix::io::Errno::OPNOTSUPP) => return Ok(()),
        Err(err) => return Err(format!("xattrs not read: {err}")),
    };

    let mut lost = Vec::new();
    let mut value = vec![0u8; 64 * 1024];

    for name in names[..len].split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let Ok(name_str) = std::str::from_utf8(name) else {
            continue;
        };
        // Only user.* and security-neutral namespaces can be set unprivileged;
        // trusted.* / system.* fail with EPERM for normal users — skip silently
        // for anything but user.* to avoid noise.
        let is_user = name_str.starts_with("user.");

        let vlen = match rustix::fs::fgetxattr(src, name_str, &mut *value) {
            Ok(v) => v,
            Err(_) if !is_user => continue,
            Err(err) => {
                lost.push(format!("{name_str} ({err})"));
                continue;
            }
        };
        match rustix::fs::fsetxattr(dest, name_str, &value[..vlen], XattrFlags::empty()) {
            Ok(()) => {}
            Err(_) if !is_user => {}
            Err(err) => lost.push(format!("{name_str} ({err})")),
        }
    }

    if lost.is_empty() {
        Ok(())
    } else {
        Err(format!("xattrs not preserved: {}", lost.join(", ")))
    }
}

/// Recreate a symlink at `dest` pointing wherever `src`'s link points.
/// Links are copied as links, never followed.
pub fn copy_symlink(src: &Path, dest: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    rustix::fs::symlinkat(&*target, rustix::fs::CWD, dest)?;
    Ok(())
}

/// Directory metadata is applied after its children are processed, since writes
/// inside would bump the times we just set.
pub fn apply_dir_meta(src_meta: &std::fs::Metadata, dest: &Path) -> MetaOutcome {
    let mut warnings = Vec::new();

    let mode = Mode::from_bits_truncate(src_meta.mode() & 0o7777);
    if let Err(err) = rustix::fs::chmod(dest, mode) {
        warnings.push(format!("dir mode not preserved: {err}"));
    }

    let times = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: Timespec {
            tv_sec: src_meta.mtime(),
            tv_nsec: src_meta.mtime_nsec(),
        },
    };
    if let Err(err) = rustix::fs::utimensat(
        rustix::fs::CWD,
        dest,
        &times,
        rustix::fs::AtFlags::empty(),
    ) {
        warnings.push(format!("dir mtime not preserved: {err}"));
    }

    MetaOutcome { warnings }
}
