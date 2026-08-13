//! Metadata preservation: mode, xattrs, mtime. Fail-soft where the destination
//! filesystem cannot hold what the source had (FAT has no mode or xattrs),
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
/// Errors collapse into a single warning string, one unreadable or unwritable
/// attribute must not fail the copy. `ENOTSUP` from the destination (FAT) is the
/// expected fail-soft case.
pub fn copy_xattrs(src: &File, dest: &File) -> Result<(), String> {
    // This runs once per file, and a tree can hold hundreds of thousands of
    // them: nearly all with no xattrs at all. Both buffers used to be 64KB
    // heap vectors allocated and zeroed up front, whatever the answer turned
    // out to be: about 11GB of memset to copy a node_modules tree, for lists
    // that were almost always empty. The empty case now touches the heap not
    // at all, and the value buffer waits until there is something to put in it.
    let mut stack = [0u8; 512];
    // Deferred: the common path never initialises it.
    let mut heap: Vec<u8>;
    let names: &[u8] = match rustix::fs::flistxattr(src, &mut stack) {
        Ok(len) => &stack[..len],
        // Source fs has no xattrs: nothing to lose.
        Err(rustix::io::Errno::OPNOTSUPP) => return Ok(()),
        // A list too long for the stack buffer. Rare enough to pay for a second
        // call: size zero asks how much room it needs.
        Err(rustix::io::Errno::RANGE) => {
            let empty: &mut [u8] = &mut [];
            let needed = rustix::fs::flistxattr(src, empty)
                .map_err(|err| format!("xattrs not read: {err}"))?;
            heap = vec![0u8; needed];
            let len = rustix::fs::flistxattr(src, &mut heap[..])
                .map_err(|err| format!("xattrs not read: {err}"))?;
            &heap[..len]
        }
        Err(err) => return Err(format!("xattrs not read: {err}")),
    };
    if names.is_empty() {
        return Ok(());
    }

    let mut lost = Vec::new();
    let mut value = vec![0u8; 64 * 1024];

    for name in names.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let Ok(name_str) = std::str::from_utf8(name) else {
            continue;
        };
        // Only user.* and security-neutral namespaces can be set unprivileged;
        // trusted.* / system.* fail with EPERM for normal users: skip silently
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
    if let Err(err) =
        rustix::fs::utimensat(rustix::fs::CWD, dest, &times, rustix::fs::AtFlags::empty())
    {
        warnings.push(format!("dir mtime not preserved: {err}"));
    }

    MetaOutcome { warnings }
}

/// Put a directory back the way it was, from what a record kept about it.
///
/// The counterpart of `apply_dir_meta` for undo, which has no source metadata
/// to copy from — only the mode and mtime noted when the directory was
/// removed. Without this, `create_dir_all` gives it 0777 & ~umask and the
/// current time, so undoing a move of a private directory silently widened it.
pub fn restore_dir_meta(dest: &Path, mode: Option<u32>, mtime: Option<(i64, i64)>) -> MetaOutcome {
    let mut warnings = Vec::new();

    if let Some(mode) = mode
        && let Err(err) = rustix::fs::chmod(dest, Mode::from_bits_truncate(mode & 0o7777))
    {
        warnings.push(format!("dir mode not restored: {err}"));
    }

    if let Some((secs, nanos)) = mtime {
        let times = Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: secs,
                tv_nsec: nanos,
            },
        };
        if let Err(err) =
            rustix::fs::utimensat(rustix::fs::CWD, dest, &times, rustix::fs::AtFlags::empty())
        {
            warnings.push(format!("dir mtime not restored: {err}"));
        }
    }

    MetaOutcome { warnings }
}
