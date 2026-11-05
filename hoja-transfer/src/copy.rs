//! File content transfer: Tier 1 (FICLONE) and Tier 2 (copy_file_range with a
//! read/write fallback), sparse-aware, cancellable, byte-accounted.

use std::fs::File;
use std::os::unix::fs::FileExt;
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
///
/// There is no length check afterwards, and there used to be one that could
/// not fail: the `ftruncate` above has already given the file its full length,
/// so measuring it proved nothing and cost an fstat per file. What does catch a
/// copy that came up short is in `Copier::extent`: a zero-byte return from
/// either mechanism before the extent is done is an error, never a success.
pub fn copy_contents(
    src: &File,
    dest: &File,
    len: u64,
    bytes_done: &AtomicU64,
    cancel: &AtomicBool,
    buf: &mut Vec<u8>,
    maybe_sparse: bool,
) -> std::io::Result<CopyOutcome> {
    rustix::fs::ftruncate(dest, len)?;
    let mut copier = Copier {
        src,
        dest,
        cfr_ok: true,
        mechanism: CopyMechanism::CopyFileRange,
        buf,
        bytes_done,
        cancel,
    };

    // A file whose allocated blocks already cover its length has no holes to
    // find, so the SEEK_DATA/SEEK_HOLE walk is two syscalls per file spent
    // proving it. That is most files, and nearly all small ones.
    if maybe_sparse {
        copier.sparse(len)?;
    } else {
        copier.extent(0, len)?;
    }

    if copier.cancelled() {
        return Ok(CopyOutcome::Cancelled);
    }
    Ok(CopyOutcome::Done(copier.mechanism))
}

/// One file's copy in progress: the two ends, which mechanism is still worth
/// trying, and the job's shared buffer and counters.
struct Copier<'a> {
    src: &'a File,
    dest: &'a File,
    /// Cleared by the first refusal. EXDEV cannot start working halfway
    /// through a file, so neither can any of the others.
    cfr_ok: bool,
    mechanism: CopyMechanism,
    /// Grown to fit, never pre-sized to the maximum. This buffer belongs to
    /// the job, not to the file, because this is the path every
    /// cross-filesystem copy takes (to a USB drive, a network mount, tmpfs),
    /// and it used to allocate a fresh 4MB for each one. Copying 66,000 files
    /// with a median size of 901 bytes asked for 265GB of buffer to move 0.7GB
    /// of data, one mmap and munmap at a time.
    buf: &'a mut Vec<u8>,
    bytes_done: &'a AtomicU64,
    cancel: &'a AtomicBool,
}

impl Copier<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Copy only the data extents of the first `len` bytes.
    fn sparse(&mut self, len: u64) -> std::io::Result<()> {
        let mut offset: u64 = 0;
        while offset < len && !self.cancelled() {
            // Find the next data extent. Filesystems without SEEK_DATA report
            // the whole remainder as data.
            let data_start = match rustix::fs::seek(self.src, rustix::fs::SeekFrom::Data(offset)) {
                Ok(pos) => pos,
                // Nothing but hole to EOF, which the ftruncate already made.
                Err(Errno::NXIO) => return Ok(()),
                Err(Errno::INVAL) | Err(Errno::OPNOTSUPP) => return self.extent(offset, len),
                Err(err) => return Err(err.into()),
            };
            if data_start >= len {
                return Ok(());
            }
            let hole_start =
                match rustix::fs::seek(self.src, rustix::fs::SeekFrom::Hole(data_start)) {
                    Ok(pos) => pos.min(len),
                    Err(_) => len,
                };
            self.extent(data_start, hole_start)?;
            offset = hole_start;
        }
        Ok(())
    }

    /// Copy bytes `start..end`, returning early (with `Ok`) on cancel; the
    /// caller checks the flag.
    fn extent(&mut self, start: u64, end: u64) -> std::io::Result<()> {
        let mut pos = start;
        while pos < end {
            if self.cancelled() {
                return Ok(());
            }
            let want = ((end - pos) as usize).min(BUF_SIZE);

            if self.cfr_ok {
                let mut off_in = pos;
                let mut off_out = pos;
                match rustix::fs::copy_file_range(
                    self.src,
                    Some(&mut off_in),
                    self.dest,
                    Some(&mut off_out),
                    want,
                ) {
                    Ok(0) => {
                        // Source shrank underneath us; surface as an error
                        // rather than spinning.
                        return Err(std::io::Error::other("copy_file_range returned 0"));
                    }
                    Ok(n) => {
                        pos += n as u64;
                        self.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
                        continue;
                    }
                    // Cross-fs-type, or fs refuses: permanent flip to
                    // read/write for this file.
                    Err(Errno::XDEV)
                    | Err(Errno::INVAL)
                    | Err(Errno::NOSYS)
                    | Err(Errno::OPNOTSUPP) => {
                        self.cfr_ok = false;
                        self.mechanism = CopyMechanism::ReadWrite;
                    }
                    Err(err) => return Err(err.into()),
                }
            }

            if self.buf.len() < want {
                self.buf.resize(want, 0);
            }
            let n = self.src.read_at(&mut self.buf[..want], pos)?;
            if n == 0 {
                return Err(std::io::Error::other("unexpected EOF while copying"));
            }
            self.dest.write_all_at(&self.buf[..n], pos)?;
            pos += n as u64;
            self.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}
