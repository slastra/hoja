//! File content transfer: Tier 1 (FICLONE) and Tier 2 (copy_file_range with a
//! read/write fallback), sparse-aware, cancellable, byte-accounted.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustix::io::Errno;

/// Which mechanism ultimately copied the bytes, for TierStats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMechanism {
    Reflink,
    CopyFileRange,
    ReadWrite,
}

pub enum CopyOutcome {
    Done(CopyMechanism),
    Cancelled,
}

const BUF_SIZE: usize = 4 * 1024 * 1024;

/// Tier 1: whole-file clone. Near-instant regardless of size; only data extents
/// are shared, so metadata still needs the normal pass.
pub fn try_reflink(src: &File, dest: &File) -> Result<(), Errno> {
    // Arg order is (dest, src).
    rustix::fs::ioctl_ficlone(dest, src)
}

/// Tier 2: sparse-aware copy of `len` bytes from `src` into `dest` (an empty
/// temp file). Holes are preserved by construction: `ftruncate` to full length
/// first (making trailing holes free), then only data extents are written.
///
/// `copy_file_range` is preferred per-extent; the first hard failure flips the
/// whole file to a reusable-buffer read/write loop at the same offsets. Short
/// returns are looped, cancellation is checked per chunk.
pub fn copy_contents(
    src: &File,
    dest: &File,
    len: u64,
    bytes_done: &AtomicU64,
    cancel: &Arc<AtomicBool>,
    buf: &mut Vec<u8>,
    maybe_sparse: bool,
) -> std::io::Result<CopyOutcome> {
    rustix::fs::ftruncate(dest, len)?;

    let mut mechanism = CopyMechanism::CopyFileRange;
    let mut cfr_ok = true;

    // A file whose allocated blocks already cover its length has no holes to
    // find, so the SEEK_DATA/SEEK_HOLE walk below is two syscalls per file
    // spent proving it. That is most files, and nearly all small ones.
    if !maybe_sparse {
        if len > 0 {
            copy_extent(
                src,
                dest,
                0,
                len,
                &mut cfr_ok,
                &mut mechanism,
                buf,
                bytes_done,
                cancel,
            )?;
            if cancel.load(Ordering::Relaxed) {
                return Ok(CopyOutcome::Cancelled);
            }
        }
        let final_len = dest.metadata()?.len();
        if final_len != len {
            return Err(std::io::Error::other(format!(
                "size mismatch after copy: expected {len}, got {final_len}"
            )));
        }
        return Ok(CopyOutcome::Done(mechanism));
    }

    let mut offset: u64 = 0;
    loop {
        // Find the next data extent. Filesystems without SEEK_DATA report the
        // whole remainder as data via the EINVAL/ENOTSUP fallback below.
        let data_start = match rustix::fs::seek(src, rustix::fs::SeekFrom::Data(offset)) {
            Ok(pos) => pos,
            Err(Errno::NXIO) => break, // nothing but hole to EOF — already truncated
            Err(Errno::INVAL) | Err(Errno::OPNOTSUPP) => {
                if offset >= len {
                    break;
                }
                // Treat the rest as one data extent.
                copy_extent(
                    src,
                    dest,
                    offset,
                    len,
                    &mut cfr_ok,
                    &mut mechanism,
                    buf,
                    bytes_done,
                    cancel,
                )?;
                if cancel.load(Ordering::Relaxed) {
                    return Ok(CopyOutcome::Cancelled);
                }
                break;
            }
            Err(err) => return Err(err.into()),
        };
        if data_start >= len {
            break;
        }

        let hole_start = match rustix::fs::seek(src, rustix::fs::SeekFrom::Hole(data_start))
        {
            Ok(pos) => pos.min(len),
            Err(_) => len,
        };

        copy_extent(
            src,
            dest,
            data_start,
            hole_start,
            &mut cfr_ok,
            &mut mechanism,
            buf,
            bytes_done,
            cancel,
        )?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(CopyOutcome::Cancelled);
        }

        offset = hole_start;
        if offset >= len {
            break;
        }
    }

    // Guard against the kernel-5.3–5.18 class of bugs where copy_file_range on
    // odd filesystems reported success while copying nothing: the temp file must
    // have exactly the source's length.
    let final_len = dest.metadata()?.len();
    if final_len != len {
        return Err(std::io::Error::other(format!(
            "size mismatch after copy: expected {len}, got {final_len}"
        )));
    }

    Ok(CopyOutcome::Done(mechanism))
}

#[allow(clippy::too_many_arguments)] // internal helper; splitting obscures the flow
fn copy_extent(
    src: &File,
    dest: &File,
    start: u64,
    end: u64,
    cfr_ok: &mut bool,
    mechanism: &mut CopyMechanism,
    buf: &mut Vec<u8>,
    bytes_done: &AtomicU64,
    cancel: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut pos = start;
    while pos < end {
        if cancel.load(Ordering::Relaxed) {
            return Ok(()); // caller checks the flag and unwinds
        }
        let want = ((end - pos) as usize).min(BUF_SIZE);

        if *cfr_ok {
            let mut off_in = pos;
            let mut off_out = pos;
            match rustix::fs::copy_file_range(
                src,
                Some(&mut off_in),
                dest,
                Some(&mut off_out),
                want,
            ) {
                Ok(0) if want > 0 => {
                    // Source shrank underneath us; surface as an error rather
                    // than spinning.
                    return Err(std::io::Error::other("copy_file_range returned 0"));
                }
                Ok(n) => {
                    pos += n as u64;
                    bytes_done.fetch_add(n as u64, Ordering::Relaxed);
                    continue;
                }
                // Cross-fs-type, or fs refuses: permanent flip to read/write
                // for this file. EXDEV can't "start working" halfway through.
                Err(Errno::XDEV) | Err(Errno::INVAL) | Err(Errno::NOSYS)
                | Err(Errno::OPNOTSUPP) => {
                    *cfr_ok = false;
                    *mechanism = CopyMechanism::ReadWrite;
                }
                Err(err) => return Err(err.into()),
            }
        }

        // Grown to fit, never pre-sized to the maximum. This buffer belongs to
        // the job, not to the file, because this is the path every
        // cross-filesystem copy takes — to a USB drive, a network mount, tmpfs
        // — and it used to allocate a fresh 4MB here for each one. Copying
        // 66,000 files with a median size of 901 bytes asked for 265GB of
        // buffer to move 0.7GB of data, one mmap and munmap at a time.
        if buf.len() < want {
            buf.resize(want, 0);
        }
        let n = src.read_at(&mut buf[..want], pos)?;
        if n == 0 {
            return Err(std::io::Error::other("unexpected EOF while copying"));
        }
        dest.write_all_at(&buf[..n], pos)?;
        pos += n as u64;
        bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(())
}
