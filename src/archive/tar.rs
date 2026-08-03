//! The tar backend, and the four things a tarball comes wrapped in.
//!
//! One implementation and five ways in, because the compression is only *which
//! reader the file is wrapped in*. That is why supporting four codecs costs
//! barely more than supporting one.
//!
//! # What makes tar different from zip
//!
//! A zip has a central directory: reading one small region at the end tells you
//! everything the archive holds. A tar has no directory at all. The only way to
//! find out what is in one is to read all of it, and when it is compressed that
//! means decompressing all of it. Measured on this machine, the largest
//! `.tar.bz2` takes about a minute.
//!
//! Two consequences run through this file:
//!
//! - **Every pass opens a fresh stream.** A decompressor cannot be rewound, so
//!   listing and extracting are separate reads from byte zero. The listing is
//!   cached (see `archive::index`) precisely so that navigating between folders
//!   inside one tarball does not pay for it again.
//! - **Members are handed over as they are found**, through `Format::list`'s
//!   callback, so a pane can show rows long before the read finishes.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;

use super::{Cancel, Counted, Failure, Format, Link, Listing, Member, Progress, Sink, tidy};

/// What a tarball is wrapped in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Plain,
    Gz,
    Bz2,
    Xz,
    Zst,
}

impl Codec {
    /// The codec a file name claims, and `None` when it does not claim to be a
    /// tarball at all.
    ///
    /// By name rather than by magic bytes, for the same reason
    /// `archive::is_archive` is: the alternative is opening every file in a
    /// listing. A name that lies is found out when it is opened.
    ///
    /// A bare `.gz` or `.zst` is deliberately absent. Those hold one file
    /// rather than a directory, and stepping into one to find a single row
    /// would be a worse answer than opening it.
    pub fn of(path: &Path) -> Option<Codec> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        // Longest first, so `.tar.gz` is not read as `.tar` with a suffix.
        for (suffix, codec) in [
            (".tar.gz", Codec::Gz),
            (".tar.bz2", Codec::Bz2),
            (".tar.xz", Codec::Xz),
            (".tar.zst", Codec::Zst),
            (".tgz", Codec::Gz),
            (".taz", Codec::Gz),
            (".tbz", Codec::Bz2),
            (".tbz2", Codec::Bz2),
            (".tz2", Codec::Bz2),
            (".txz", Codec::Xz),
            (".tzst", Codec::Zst),
            (".tar", Codec::Plain),
        ] {
            if name.ends_with(suffix) && name.len() > suffix.len() {
                return Some(codec);
            }
        }
        None
    }
}

pub struct Tar {
    path: std::path::PathBuf,
    codec: Codec,
}

impl Tar {
    /// Nothing is read here.
    ///
    /// Unlike a zip, whose central directory is parsed at open and so finds out
    /// there and then that a file is not an archive, a tarball has nothing to
    /// check without reading it. A file named `.tar.gz` that is not one fails
    /// on the first pass instead.
    pub fn open(path: &Path, codec: Codec) -> anyhow::Result<Self> {
        // Opened and dropped, so an unreadable file says so now rather than
        // from inside a background thread three layers down.
        File::open(path).with_context(|| format!("{}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            codec,
        })
    }

    /// A fresh decompressed stream from byte zero.
    ///
    /// `read` counts the *compressed* bytes taken off the file, which is the
    /// only figure that climbs smoothly whatever the archive holds: the members
    /// can be thirty enormous ones and the file is still consumed steadily.
    fn stream(&self, read: Option<&Arc<Progress>>) -> anyhow::Result<Box<dyn Read + Send>> {
        let file = Counting {
            inner: File::open(&self.path)?,
            read: read.cloned(),
        };
        // 64 KiB, because every one of these reads straight through. There is
        // no seeking here for a small buffer to save, which is the trade the
        // zip backend has to make.
        let buffered = BufReader::with_capacity(64 * 1024, file);
        Ok(match self.codec {
            Codec::Plain => Box::new(buffered),
            Codec::Gz => Box::new(flate2::read::GzDecoder::new(buffered)),
            Codec::Bz2 => Box::new(bzip2::read::BzDecoder::new(buffered)),
            Codec::Zst => Box::new(
                ruzstd::decoding::StreamingDecoder::new(buffered)
                    .map_err(|err| anyhow::anyhow!("{err}"))?,
            ),
            Codec::Xz => Box::new(piped(self.path.clone(), read.cloned())?),
        })
    }
}

/// `lzma-rs` on a thread of its own, writing into a pipe.
///
/// It is the one codec here with no `Read` implementation: its whole API is
/// `xz_decompress(input, output)` over complete buffers, and the largest
/// `.tar.xz` on this machine expands from 333 MB to 2 GB. Holding that in
/// memory to list an archive is not a trade worth making, so the decode runs
/// beside us and the pipe bounds what exists at once to its own buffer.
///
/// It also gets cancellation for nothing: when the reader is dropped the write
/// fails, and the thread unwinds on its own.
///
/// Rejected: the `xz` crate is pure Rust with a real streaming reader and looks
/// faster, but at 0.4.6-rc.0 with three published versions it is not what a
/// file manager should decompress with. Worth revisiting.
fn piped(path: std::path::PathBuf, read: Option<Arc<Progress>>) -> anyhow::Result<PipedDecoder> {
    let (reader, mut writer) = std::io::pipe()?;
    let fault = std::sync::Arc::new(std::sync::Mutex::new(None));
    let theirs = std::sync::Arc::clone(&fault);

    std::thread::Builder::new()
        .name("hoja-xz".to_string())
        .spawn(move || {
            let result = File::open(&path)
                .map_err(|err| err.to_string())
                .and_then(|file| {
                    let mut input =
                        BufReader::with_capacity(64 * 1024, Counting { inner: file, read });
                    lzma_rs::xz_decompress(&mut input, &mut writer).map_err(|err| err.to_string())
                });
            if let Err(err) = result
                && let Ok(mut slot) = theirs.lock()
            {
                *slot = Some(err);
            }
        })?;

    Ok(PipedDecoder { reader, fault })
}

/// The reading half of `piped`, plus whatever went wrong at the writing end.
///
/// The fault matters more than it looks. A decode that fails part-way closes
/// the pipe, which reaches a plain reader as a clean end of file, and a
/// truncated `.tar.xz` would then read as a short but perfectly valid tarball:
/// a listing missing half its rows and saying nothing about it. So the end of
/// the stream is where the other thread's verdict is collected.
struct PipedDecoder {
    reader: std::io::PipeReader,
    fault: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl Read for PipedDecoder {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.reader.read(buf)?;
        if read == 0
            && let Ok(slot) = self.fault.lock()
            && let Some(err) = slot.as_ref()
        {
            return Err(std::io::Error::other(err.clone()));
        }
        Ok(read)
    }
}

/// A reader that says how much has come off the file behind it.
struct Counting<R> {
    inner: R,
    read: Option<Arc<Progress>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(progress) = &self.read {
            progress.bytes.fetch_add(read as u64, Ordering::Relaxed);
        }
        Ok(read)
    }
}

/// Everything about one entry that this cares about.
///
/// A `None` means the entry is not one to show: an extended header, a device,
/// a fifo, or a name `tidy` refused.
fn member_of<R: Read>(entry: &tar::Entry<'_, R>) -> Option<Member> {
    let kind = entry.header().entry_type();
    // The GNU and PAX extension headers are how long names and big sizes are
    // carried. The `tar` crate has already folded them into the entry that
    // follows, so they are bookkeeping rather than content.
    if kind.is_gnu_longname()
        || kind.is_gnu_longlink()
        || kind.is_pax_global_extensions()
        || kind.is_pax_local_extensions()
    {
        return None;
    }

    let path = tidy(&entry.path().ok()?.to_string_lossy())?;
    let is_dir = kind.is_dir();

    let link = if kind.is_symlink() || kind.is_hard_link() {
        let target = entry.link_name().ok()??.to_string_lossy().into_owned();
        if target.is_empty() {
            return None;
        }
        Some(if kind.is_symlink() {
            Link::Sym(target)
        } else {
            // Relative to the archive's root, and safe for the same reason
            // every other path here is.
            Link::Hard(tidy(&target)?)
        })
    } else {
        // A device, a fifo or a socket. There is a name and there is nothing
        // behind it, and hoja is not going to mknod.
        if !kind.is_file() && !is_dir && !kind.is_contiguous() {
            return None;
        }
        None
    };

    Some(Member {
        path,
        is_dir,
        link,
        size: if is_dir { 0 } else { entry.size() },
        // A real Unix timestamp, unlike a zip's, so there is nothing to guess
        // about a timezone.
        modified: entry
            .header()
            .mtime()
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        mode: entry.header().mode().ok(),
        // Every entry in a tarball is readable if the stream is: there is no
        // per-member codec the way a zip has.
        readable: true,
    })
}

impl Format for Tar {
    fn list(
        &mut self,
        cancel: &Cancel,
        progress: &Arc<Progress>,
        found: &mut dyn FnMut(Member),
    ) -> anyhow::Result<usize> {
        let mut archive = tar::Archive::new(self.stream(Some(progress))?);
        let mut skipped = 0;

        for entry in archive.entries()? {
            if cancel.stopped() {
                anyhow::bail!("cancelled");
            }
            // An error here is the stream itself going wrong, which is how a
            // file that is not a tarball and a truncated one both arrive.
            let entry = entry?;
            match member_of(&entry) {
                Some(member) => found(member),
                None => skipped += 1,
            }
        }

        Ok(skipped)
    }

    fn extract(
        &mut self,
        listing: &Listing,
        wanted: &BTreeSet<usize>,
        sink: &mut dyn Sink,
        progress: &Progress,
        cancel: &Cancel,
    ) -> anyhow::Result<Vec<Failure>> {
        // By path rather than by index: the listing has had refused names
        // dropped out of it, so its numbering is not the archive's. One pass
        // matches as it walks, which is the whole reason this takes a set.
        let by_path: std::collections::HashSet<&str> = wanted
            .iter()
            .filter_map(|ix| listing.members.get(*ix))
            .map(|member| member.path.as_str())
            .collect();
        if by_path.is_empty() {
            return Ok(Vec::new());
        }

        let mut failures = Vec::new();
        let mut left = by_path.len();
        let mut archive = tar::Archive::new(self.stream(None)?);

        for entry in archive.entries()? {
            if cancel.stopped() {
                anyhow::bail!("cancelled");
            }
            // Everything asked for has been handed over, and the rest of the
            // stream is of no interest. Stopping here is what makes taking one
            // file out of the front of a large tarball quick.
            if left == 0 {
                break;
            }

            let mut entry = entry?;
            let Some(member) = member_of(&entry) else {
                continue;
            };
            if !by_path.contains(member.path.as_str()) {
                continue;
            }
            left -= 1;

            let result = if member.is_dir {
                sink.dir(&member)
            } else if let Some(link) = &member.link {
                sink.link(&member, link)
            } else {
                let mut counted = Counted {
                    inner: &mut entry,
                    progress,
                    cancel,
                };
                sink.file(&member, &mut counted)
            };

            match result {
                Ok(()) => {
                    progress.files.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    // A cancelled read arrives here, through the counting
                    // reader. The pass is what the caller wanted stopped.
                    if cancel.stopped() {
                        anyhow::bail!("cancelled");
                    }
                    failures.push(Failure {
                        path: member.path.clone(),
                        reason: err.to_string(),
                    });
                }
            }
        }

        Ok(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{Index, collect};
    use std::io::Write;

    /// A tarball in a temp file, wrapped in `codec`.
    ///
    /// Written with the crates the binary reads with, so a fixture cannot pass
    /// by testing a tool that is not the one shipped. `ruzstd` decodes only, so
    /// zstd has a fixture of its own further down.
    fn write(entries: &[(&str, &[u8])], codec: Codec) -> tempfile::NamedTempFile {
        let suffix = match codec {
            Codec::Plain => ".tar",
            Codec::Gz => ".tar.gz",
            Codec::Bz2 => ".tar.bz2",
            Codec::Xz => ".tar.xz",
            Codec::Zst => ".tar.zst",
        };
        let file = tempfile::Builder::new()
            .suffix(suffix)
            .tempfile()
            .expect("temp file");

        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (name, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(if name.ends_with('/') { 0o755 } else { 0o644 });
                header.set_mtime(1_700_000_000);
                header.set_entry_type(if name.ends_with('/') {
                    tar::EntryType::Directory
                } else {
                    tar::EntryType::Regular
                });
                header.set_cksum();
                builder
                    .append_data(&mut header, name, *body)
                    .expect("append");
            }
            builder.finish().expect("finish");
        }

        let mut out = file.reopen().expect("reopen");
        match codec {
            Codec::Plain => out.write_all(&raw).unwrap(),
            Codec::Gz => {
                let mut enc =
                    flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
                enc.write_all(&raw).unwrap();
                enc.finish().unwrap();
            }
            Codec::Bz2 => {
                let mut enc = bzip2::write::BzEncoder::new(&mut out, bzip2::Compression::fast());
                enc.write_all(&raw).unwrap();
                enc.finish().unwrap();
            }
            Codec::Xz => {
                let mut packed = Vec::new();
                lzma_rs::xz_compress(&mut std::io::Cursor::new(&raw), &mut packed).unwrap();
                out.write_all(&packed).unwrap();
            }
            Codec::Zst => unreachable!("ruzstd decodes only; use the committed fixture"),
        }
        out.flush().unwrap();
        file
    }

    fn listing_of(path: &Path, codec: Codec) -> Listing {
        collect(&mut Tar::open(path, codec).unwrap(), &Cancel::new()).expect("list")
    }

    fn shape(listing: &Listing) -> Vec<(String, bool, u64)> {
        listing
            .members
            .iter()
            .map(|m| (m.path.clone(), m.is_dir, m.size))
            .collect()
    }

    const SAMPLE: &[(&str, &[u8])] = &[
        ("ttf/", b""),
        ("ttf/Inter.ttf", b"inter"),
        ("ttf/sub/Mono.ttf", b"mono"),
        ("README", b"hello"),
    ];

    #[test]
    fn a_name_says_which_codec_it_is() {
        let of = |name: &str| Codec::of(Path::new(name));
        assert_eq!(of("x.tar"), Some(Codec::Plain));
        assert_eq!(of("x.tar.gz"), Some(Codec::Gz));
        assert_eq!(of("x.tgz"), Some(Codec::Gz));
        assert_eq!(of("x.tar.bz2"), Some(Codec::Bz2));
        assert_eq!(of("x.tbz2"), Some(Codec::Bz2));
        assert_eq!(of("x.tar.xz"), Some(Codec::Xz));
        assert_eq!(of("x.txz"), Some(Codec::Xz));
        assert_eq!(of("x.tar.zst"), Some(Codec::Zst));
        assert_eq!(of("x.tzst"), Some(Codec::Zst));
        // Case, because a download can arrive shouting.
        assert_eq!(of("X.TAR.GZ"), Some(Codec::Gz));

        // One file, not a directory. Opening it is the right answer.
        assert_eq!(of("notes.txt.gz"), None);
        assert_eq!(of("blob.zst"), None);
        assert_eq!(of("x.zip"), None);
        // A file called nothing but the suffix is not one of these either.
        assert_eq!(of(".tar.gz"), None);
    }

    #[test]
    fn every_codec_lists_the_same_tarball() {
        let plain = shape(&listing_of(
            write(SAMPLE, Codec::Plain).path(),
            Codec::Plain,
        ));
        assert_eq!(
            plain,
            [
                ("ttf".to_string(), true, 0),
                ("ttf/Inter.ttf".to_string(), false, 5),
                ("ttf/sub/Mono.ttf".to_string(), false, 4),
                ("README".to_string(), false, 5),
            ]
        );
        for codec in [Codec::Gz, Codec::Bz2, Codec::Xz] {
            let file = write(SAMPLE, codec);
            assert_eq!(
                shape(&listing_of(file.path(), codec)),
                plain,
                "{codec:?} disagreed with the uncompressed one"
            );
        }
    }

    #[test]
    fn folders_the_tarball_never_named_are_synthesised() {
        // `ttf/sub` has no entry of its own here, exactly as in the zip case.
        let file = write(SAMPLE, Codec::Gz);
        let index = Index::build(listing_of(file.path(), Codec::Gz));
        let names: Vec<_> = index
            .rows(Path::new("ttf"))
            .unwrap()
            .iter()
            .map(|r| (r.name.as_str(), r.is_dir, r.size))
            .collect();
        assert_eq!(names, [("Inter.ttf", false, 5), ("sub", true, 4)]);
    }

    #[test]
    fn a_long_name_survives_the_gnu_extension_header() {
        // Over 100 bytes, so it cannot fit the header field and `tar` writes a
        // `GNUlongname` entry ahead of it. That entry is bookkeeping and must
        // not turn up as a row.
        let long = format!("deep/{}/file.txt", "n".repeat(160));
        let file = write(&[(long.as_str(), b"x")], Codec::Gz);
        let listing = listing_of(file.path(), Codec::Gz);
        assert_eq!(listing.members.len(), 1, "the header entry became a row");
        assert_eq!(listing.members[0].path, long);
    }

    #[test]
    fn a_file_that_is_not_a_tarball_fails() {
        let mut text = tempfile::Builder::new()
            .suffix(".tar.gz")
            .tempfile()
            .unwrap();
        text.write_all(b"not a tarball at all\n").unwrap();
        text.flush().unwrap();
        assert!(
            collect(
                &mut Tar::open(text.path(), Codec::Gz).unwrap(),
                &Cancel::new()
            )
            .is_err()
        );
    }

    #[test]
    fn a_truncated_xz_is_an_error_and_not_a_short_listing() {
        // The specific thing `PipedDecoder`'s fault slot exists for. A decode
        // that stops half way closes the pipe, which without it reaches the tar
        // parser as a clean end of stream: a listing missing most of its rows
        // and saying nothing about it.
        let entries: Vec<(String, Vec<u8>)> = (0..200)
            .map(|i| (format!("file{i:03}.bin"), vec![b'x'; 4096]))
            .collect();
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let full = write(&borrowed, Codec::Xz);

        let whole = std::fs::read(full.path()).unwrap();
        assert_eq!(listing_of(full.path(), Codec::Xz).members.len(), 200);

        let cut = tempfile::Builder::new()
            .suffix(".tar.xz")
            .tempfile()
            .unwrap();
        std::fs::write(cut.path(), &whole[..whole.len() / 2]).unwrap();

        let result = collect(
            &mut Tar::open(cut.path(), Codec::Xz).unwrap(),
            &Cancel::new(),
        );
        assert!(result.is_err(), "a truncated xz read as a valid tarball");
    }

    /// One tar entry, header bytes and all.
    ///
    /// `tar::Builder` refuses to write a name containing `..`, which is the
    /// right call for a writer and leaves no way to build the fixture that
    /// matters here. So this lays out the 512 byte ustar header by hand, which
    /// is what a hostile archive is anyway: somebody wrote those bytes on
    /// purpose, and no library stopped them.
    fn raw_entry(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let put = |header: &mut [u8; 512], at: usize, text: &[u8]| {
            header[at..at + text.len()].copy_from_slice(text);
        };
        put(&mut header, 0, name.as_bytes());
        put(&mut header, 100, b"0000644\0");
        put(&mut header, 108, b"0000000\0");
        put(&mut header, 116, b"0000000\0");
        put(
            &mut header,
            124,
            format!("{:011o}\0", body.len()).as_bytes(),
        );
        put(&mut header, 136, b"00000000000\0");
        header[156] = b'0'; // a regular file
        // "ustar", NUL, then the version "00". Spelled out because the
        // obvious escape reads as an octal one.
        put(
            &mut header,
            257,
            &[b'u', b's', b't', b'a', b'r', 0, b'0', b'0'],
        );

        // The checksum is over the whole header with its own field blanked.
        put(&mut header, 148, b"        ");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(512), 0);
        out
    }

    #[test]
    fn a_hostile_name_never_reaches_the_listing() {
        let mut raw = raw_entry("../escaped.txt", b"no");
        raw.extend(raw_entry("fine.txt", b"ok"));
        raw.extend([0u8; 1024]); // the two empty blocks that end an archive

        let file = tempfile::Builder::new().suffix(".tar").tempfile().unwrap();
        std::fs::write(file.path(), &raw).unwrap();

        let listing = listing_of(file.path(), Codec::Plain);
        assert_eq!(listing.members.len(), 1, "{:?}", shape(&listing));
        assert_eq!(listing.members[0].path, "fine.txt");
        assert_eq!(listing.skipped, 1);
    }

    /// The one codec whose crate cannot write.
    ///
    /// `ruzstd` decodes only, so this fixture is committed rather than built:
    /// 174 bytes, holding the same tree as `SAMPLE`, written by the `zstd`
    /// tool as a single frame like every real `.tar.zst` on this machine.
    const ZSTD_SAMPLE: &[u8] = include_bytes!("testdata/sample.tar.zst");

    #[test]
    fn a_real_zstd_tarball_lists() {
        let file = tempfile::Builder::new()
            .suffix(".tar.zst")
            .tempfile()
            .unwrap();
        std::fs::write(file.path(), ZSTD_SAMPLE).unwrap();
        let listing = listing_of(file.path(), Codec::Zst);
        let paths: Vec<_> = listing.members.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "ttf",
                "ttf/Inter.ttf",
                "ttf/sub",
                "ttf/sub/Mono.ttf",
                "README"
            ]
        );
    }

    /// Collects what it is handed, so a test can assert on bytes and links.
    #[derive(Default)]
    struct Collect {
        files: Vec<(String, Vec<u8>)>,
        dirs: Vec<String>,
        links: Vec<(String, Link)>,
        opens: usize,
    }

    impl Sink for Collect {
        fn dir(&mut self, member: &Member) -> std::io::Result<()> {
            self.dirs.push(member.path.clone());
            Ok(())
        }
        fn link(&mut self, member: &Member, link: &Link) -> std::io::Result<()> {
            self.links.push((member.path.clone(), link.clone()));
            Ok(())
        }
        fn file(&mut self, member: &Member, body: &mut dyn Read) -> std::io::Result<()> {
            self.opens += 1;
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes)?;
            self.files.push((member.path.clone(), bytes));
            Ok(())
        }
    }

    fn pull(path: &Path, codec: Codec, want: &[&str]) -> (Collect, Vec<Failure>) {
        let listing = listing_of(path, codec);
        let wanted: BTreeSet<usize> = listing
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| want.contains(&m.path.as_str()))
            .map(|(ix, _)| ix)
            .collect();
        let mut sink = Collect::default();
        let failures = Tar::open(path, codec)
            .unwrap()
            .extract(
                &listing,
                &wanted,
                &mut sink,
                &Progress::default(),
                &Cancel::new(),
            )
            .expect("the pass itself succeeds");
        (sink, failures)
    }

    #[test]
    fn a_set_of_members_comes_out_in_one_pass() {
        let file = write(SAMPLE, Codec::Gz);
        let (sink, failures) = pull(
            file.path(),
            Codec::Gz,
            &["ttf/Inter.ttf", "ttf/sub/Mono.ttf", "README"],
        );
        assert!(failures.is_empty());
        // Once per wanted member and no more, which is the whole reason
        // `extract` takes a set rather than being called three times.
        assert_eq!(sink.opens, 3);
        assert_eq!(
            sink.files,
            [
                ("ttf/Inter.ttf".to_string(), b"inter".to_vec()),
                ("ttf/sub/Mono.ttf".to_string(), b"mono".to_vec()),
                ("README".to_string(), b"hello".to_vec()),
            ]
        );
    }

    /// A tarball holding both kinds of link.
    fn with_links() -> tempfile::NamedTempFile {
        let file = tempfile::Builder::new().suffix(".tar").tempfile().unwrap();
        let mut out = file.reopen().unwrap();
        {
            let mut builder = tar::Builder::new(&mut out);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/real", &b"hello"[..])
                .unwrap();

            let mut sym = tar::Header::new_gnu();
            sym.set_entry_type(tar::EntryType::Symlink);
            sym.set_size(0);
            sym.set_mtime(1_700_000_000);
            sym.set_cksum();
            builder.append_link(&mut sym, "bin/soft", "real").unwrap();

            let mut hard = tar::Header::new_gnu();
            hard.set_entry_type(tar::EntryType::Link);
            hard.set_size(0);
            hard.set_mtime(1_700_000_000);
            hard.set_cksum();
            builder
                .append_link(&mut hard, "bin/hard", "bin/real")
                .unwrap();
            builder.finish().unwrap();
        }
        out.flush().unwrap();
        file
    }

    #[test]
    fn a_link_lists_as_a_link_and_comes_out_as_one() {
        // Toolchain tarballs are full of these, and a copy that turned every
        // symlink into a small text file holding a path is the sort of wrong
        // that only shows up much later.
        let file = with_links();
        let listing = listing_of(file.path(), Codec::Plain);
        let links: Vec<_> = listing
            .members
            .iter()
            .filter_map(|m| m.link.as_ref().map(|l| (m.path.as_str(), l.clone())))
            .collect();
        assert_eq!(
            links,
            [
                ("bin/soft", Link::Sym("real".to_string())),
                ("bin/hard", Link::Hard("bin/real".to_string())),
            ]
        );

        let (sink, failures) = pull(file.path(), Codec::Plain, &["bin/soft", "bin/hard"]);
        assert!(failures.is_empty());
        assert!(sink.files.is_empty(), "a link has no bytes to hand over");
        assert_eq!(
            sink.links,
            [
                ("bin/soft".to_string(), Link::Sym("real".to_string())),
                ("bin/hard".to_string(), Link::Hard("bin/real".to_string())),
            ]
        );
    }

    #[test]
    fn a_hard_link_without_its_target_fails_alone() {
        // What `tar -x` does: a hard link is a second name for a file, so
        // without the first name there is nothing to give it.
        let file = with_links();
        let dest = tempfile::tempdir().unwrap();
        let failures = crate::archive::extract(
            file.path(),
            Path::new(""),
            &["bin/hard".to_string()],
            dest.path(),
            &Progress::default(),
            &Cancel::new(),
        )
        .expect("the pass itself succeeds");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, "bin/hard");

        // And with the target, it works.
        let dest = tempfile::tempdir().unwrap();
        let failures = crate::archive::extract(
            file.path(),
            Path::new(""),
            &["bin".to_string()],
            dest.path(),
            &Progress::default(),
            &Cancel::new(),
        )
        .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            std::fs::read(dest.path().join("bin/hard")).unwrap(),
            b"hello"
        );
        let soft = std::fs::symlink_metadata(dest.path().join("bin/soft")).unwrap();
        assert!(soft.file_type().is_symlink(), "the symlink became a file");
        assert_eq!(
            std::fs::read_link(dest.path().join("bin/soft")).unwrap(),
            Path::new("real")
        );
    }

    #[test]
    fn cancelling_stops_a_listing() {
        let file = write(SAMPLE, Codec::Gz);
        let cancel = Cancel::new();
        cancel.stop();
        assert!(collect(&mut Tar::open(file.path(), Codec::Gz).unwrap(), &cancel).is_err());
    }
}
