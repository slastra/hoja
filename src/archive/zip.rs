//! The zip backend.
//!
//! Reads through the `zip` crate with only the deflate codec compiled in, and
//! the crate refers to itself as `::zip` throughout because this module is also
//! called `zip`.
//!
//! Two behaviours are worth knowing about before reading the code:
//!
//! - **A member compressed with something not built in is still listed.** Its
//!   name and its real size come from the central directory, which does not
//!   need a codec. Only asking for its contents fails, and it fails as one
//!   member rather than as the archive.
//! - **A file that is not a zip fails at `open`.** Five of the 127 files named
//!   `.zip` on one real machine are truncated downloads or placeholder text, so
//!   this is a case the interface has to handle rather than an exotic one.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::Ordering;

use ::zip::CompressionMethod;
use ::zip::read::ZipArchive;
use anyhow::Context as _;

use super::{
    Cancel, Counted, Failure, Format, Link, Listing, Member, Progress, Sink, Use, from_civil, tidy,
};

pub struct Zip {
    archive: ZipArchive<BufReader<File>>,
}

impl Zip {
    /// Read the central directory, which is where a file that is not a zip is
    /// found out.
    ///
    /// `using` picks the buffer, and the difference is not small. Listing seeks
    /// to every member's local header, so the buffer is thrown away once per
    /// entry: a large one is read and discarded as many times as the archive
    /// has members. Extraction reads each member straight through, which is
    /// exactly what a large one is for. Measured on a 32,664 member, 1.76 GB
    /// archive:
    ///
    /// |         | list  | extract |
    /// |---------|-------|---------|
    /// |  4 KiB  |  76ms |   4.63s |
    /// | 64 KiB  | 176ms |   4.08s |
    ///
    /// Nothing does both through one handle, so there is no compromise to
    /// strike: `archive::index` opens to list and drops it, and an extraction
    /// opens again.
    pub fn open(path: &Path, using: Use) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("{}", path.display()))?;
        let buffer = match using {
            Use::List => 4 * 1024,
            Use::Read => 64 * 1024,
        };
        let archive = ZipArchive::new(BufReader::with_capacity(buffer, file))
            .with_context(|| format!("{} is not a zip file", path.display()))?;
        Ok(Self { archive })
    }
}

/// Whether this build can decompress it.
///
/// Stored and deflate are the two the feature set compiles in, and between them
/// they were 50,256 of the 50,263 member entries across the zip files on one
/// real machine. The other seven were a zstd test fixture inside a crate.
fn readable(method: CompressionMethod) -> bool {
    matches!(
        method,
        CompressionMethod::Stored | CompressionMethod::Deflated
    )
}

impl Format for Zip {
    fn list(
        &mut self,
        cancel: &Cancel,
        progress: &std::sync::Arc<Progress>,
        found: &mut dyn FnMut(Member),
    ) -> anyhow::Result<usize> {
        let mut skipped = 0;

        for ix in 0..self.archive.len() {
            // Once per member rather than once per archive: a listing is the
            // only slow thing a navigation into an archive does, and this is
            // what lets the navigation that supersedes it stop this one.
            if cancel.stopped() {
                anyhow::bail!("cancelled");
            }

            // Everything the central directory knows, read through a handle
            // that decodes nothing. `by_index_raw` does seek to each member's
            // local header, so this is a seek per entry rather than a single
            // read of the whole directory. A member whose local header is
            // unreadable is skipped rather than failing the archive: a zip can
            // be damaged in one place and fine everywhere else.
            let Some(member) = ({
                let Ok(file) = self.archive.by_index_raw(ix) else {
                    skipped += 1;
                    continue;
                };
                let Some(path) = tidy(file.name()) else {
                    skipped += 1;
                    continue;
                };
                // A trailing `/` is how a zip names a directory; the unix mode
                // says so too where the archive was written on a unix.
                let is_dir = file.is_dir();
                Some(Member {
                    path,
                    is_dir,
                    // Filled in below, for the few members that are links.
                    link: None,
                    size: if is_dir { 0 } else { file.size() },
                    // The extended timestamp is a real Unix time and needs no
                    // guessing about a timezone, so it wins wherever it is
                    // present, which is anything written this century. The DOS
                    // date in the header is the fallback: see `from_civil`.
                    modified: unix_time(&file).or_else(|| {
                        file.last_modified().map(|at| {
                            from_civil(
                                i64::from(at.year()),
                                u32::from(at.month()),
                                u32::from(at.day()),
                                u32::from(at.hour()),
                                u32::from(at.minute()),
                                u32::from(at.second()),
                            )
                        })
                    }),
                    mode: file.unix_mode(),
                    readable: readable(file.compression()),
                })
            }) else {
                continue;
            };

            // A zip keeps a symlink's target in the member's *body*, so reading
            // it needs a decoding handle, which the one above deliberately is
            // not. Looked up a second time rather than complicating the common
            // path: one symlink turned up across 127 real archives, and without
            // this it would come out of a copy as a small text file holding a
            // path.
            let link = is_symlink(&member)
                .then(|| target_of(&mut self.archive, ix))
                .flatten();

            // A zip's central directory is read in one go, so this is
            // never a progress line anyone sees. Kept honest anyway.
            progress.bytes.fetch_add(member.size, Ordering::Relaxed);
            found(Member { link, ..member });
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
        let mut failures = Vec::new();

        // Ascending, which is the order the archive stores them in and so
        // roughly the order they sit on the disk. A `BTreeSet` gives that for
        // nothing, which is half the reason the parameter is one.
        for &ix in wanted {
            if cancel.stopped() {
                anyhow::bail!("cancelled");
            }
            let Some(member) = listing.members.get(ix) else {
                continue;
            };

            if member.is_dir {
                if let Err(err) = sink.dir(member) {
                    failures.push(Failure {
                        path: member.path.clone(),
                        reason: err.to_string(),
                    });
                }
                continue;
            }

            if let Some(link) = &member.link {
                if let Err(err) = sink.link(member, link) {
                    failures.push(Failure {
                        path: member.path.clone(),
                        reason: err.to_string(),
                    });
                }
                continue;
            }

            if !member.readable {
                failures.push(Failure {
                    path: member.path.clone(),
                    reason: "compressed with a method hoja was not built with".to_string(),
                });
                continue;
            }

            // Looked up by name rather than by index: `wanted` indexes the
            // listing, and the listing has had refused and repeated names
            // dropped out of it, so the two are not the same numbering.
            let Some(at) = self.archive.index_for_name(&member.path) else {
                failures.push(Failure {
                    path: member.path.clone(),
                    reason: "no longer in the archive".to_string(),
                });
                continue;
            };

            let result = self
                .archive
                .by_index(at)
                .map_err(anyhow::Error::from)
                .and_then(|body| {
                    let mut counted = Counted {
                        inner: body,
                        progress,
                        cancel,
                    };
                    sink.file(member, &mut counted).map_err(anyhow::Error::from)
                });

            match result {
                Ok(()) => {
                    progress.files.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    // A cancelled read reaches here as a per-member failure,
                    // because it arrives through the reader. The pass as a whole
                    // is what the caller wanted stopped, so say so.
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

/// Whether the mode the archive recorded says this is a symbolic link.
///
/// `S_IFLNK`. A zip written on Windows carries no mode at all, and cannot hold
/// a symlink either, so the answer there is no.
fn is_symlink(member: &Member) -> bool {
    member.mode.is_some_and(|mode| mode & 0o170000 == 0o120000)
}

/// Where a symlink member points, read from its body.
///
/// Capped: a target is a few dozen bytes, and a member claiming to be a link to
/// four kilobytes of something is not one worth believing.
fn target_of(archive: &mut ZipArchive<BufReader<File>>, ix: usize) -> Option<Link> {
    let mut target = String::new();
    archive
        .by_index(ix)
        .ok()?
        .take(4096)
        .read_to_string(&mut target)
        .ok()?;
    (!target.is_empty()).then_some(Link::Sym(target))
}

/// The member's real modification time, where the archive carries one.
///
/// The extended timestamp field is what anything written since about 1997 puts
/// in a zip alongside the DOS date, and unlike the DOS date it is a Unix time
/// with no timezone left to guess at.
fn unix_time<R: std::io::Read>(
    file: &::zip::read::ZipFile<'_, R>,
) -> Option<std::time::SystemTime> {
    file.extra_data_fields().find_map(|field| match field {
        ::zip::ExtraField::ExtendedTimestamp(stamp) => stamp
            .mod_time()
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(secs))),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Index;
    use std::io::{Read, Write};

    /// Collects what it is given, so a test can assert on the bytes rather than
    /// on a directory somewhere.
    #[derive(Default)]
    struct Collect {
        files: Vec<(String, Vec<u8>)>,
        dirs: Vec<String>,
        links: Vec<(String, Link)>,
        /// How many times a body was opened, which is what says whether the
        /// pass really was one pass.
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

    /// Everything in an archive, which is what most of these want.
    fn listing_of(path: &Path) -> Listing {
        crate::archive::collect(&mut Zip::open(path, Use::List).unwrap(), &Cancel::new())
            .expect("list")
    }

    /// A zip in a temporary file, built by the crate's own writer.
    ///
    /// Only stored and deflate, because those are the only two this build has a
    /// codec for and a test cannot write what the binary cannot read.
    fn write(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        write_with(entries, CompressionMethod::Deflated)
    }

    fn write_with(entries: &[(&str, &[u8])], method: CompressionMethod) -> tempfile::NamedTempFile {
        // Named `.zip`, because `archive::open` picks the format by the name
        // and the entry point copy-out goes through is that one.
        let file = tempfile::Builder::new()
            .suffix(".zip")
            .tempfile()
            .expect("temp file");
        let mut writer = ::zip::ZipWriter::new(file.reopen().expect("reopen"));
        let options: ::zip::write::FileOptions<'_, ()> =
            ::zip::write::FileOptions::default().compression_method(method);
        for (name, body) in entries {
            if name.ends_with('/') {
                writer.add_directory(*name, options).expect("add dir");
            } else {
                writer.start_file(*name, options).expect("start file");
                writer.write_all(body).expect("write");
            }
        }
        writer.finish().expect("finish");
        file
    }

    #[test]
    fn a_file_that_is_not_a_zip_fails_cleanly() {
        // Five of 127 files named `.zip` on one real machine are like this:
        // truncated downloads and a text placeholder. Not exotic.
        let mut text = tempfile::NamedTempFile::new().unwrap();
        text.write_all(b"placeholder zip\n").unwrap();
        let err = match Zip::open(text.path(), Use::List) {
            Ok(_) => panic!("a text file must not open as a zip"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("not a zip file"),
            "unhelpful: {err}"
        );

        // The other shape: a download that stopped, so nothing but zeros.
        let mut zeros = tempfile::NamedTempFile::new().unwrap();
        zeros.write_all(&[0u8; 4096]).unwrap();
        assert!(Zip::open(zeros.path(), Use::List).is_err());
    }

    #[test]
    fn a_zip_with_no_directory_entries_still_lists_its_folders() {
        let file = write(&[
            ("ttf/Inter.ttf", b"inter"),
            ("ttf/sub/Mono.ttf", b"mono"),
            ("README", b"hello"),
        ]);
        let listing = listing_of(file.path());
        // The writer stored no directories, which is what is being tested.
        assert!(listing.members.iter().all(|m| !m.is_dir));

        let index = Index::build(listing);
        let root: Vec<_> = index
            .rows(Path::new(""))
            .unwrap()
            .iter()
            .map(|r| (r.name.as_str(), r.is_dir, r.size))
            .collect();
        assert_eq!(root, [("README", false, 5), ("ttf", true, 9)]);
    }

    #[test]
    fn a_hostile_name_never_reaches_the_listing() {
        let file = write(&[("../../etc/passwd", b"nope"), ("fine.txt", b"ok")]);
        let listing = listing_of(file.path());
        assert_eq!(listing.members.len(), 1);
        assert_eq!(listing.members[0].path, "fine.txt");
        assert_eq!(listing.skipped, 1);
    }

    #[test]
    fn only_the_two_codecs_this_build_has_count_as_readable() {
        assert!(readable(CompressionMethod::Stored));
        assert!(readable(CompressionMethod::Deflated));
        // The seven odd members in the survey. Without the feature the variant
        // is `Unsupported(93)`, which is exactly how they would be read here.
        assert!(!readable(CompressionMethod::ZSTD));
        assert!(!readable(CompressionMethod::BZIP2));
        assert!(!readable(CompressionMethod::LZMA));
    }

    #[test]
    fn a_member_this_build_cannot_decompress_fails_alone() {
        // The listing keeps it, with its name and its real size, because
        // neither of those needed a codec. Only asking for the bytes fails, and
        // it fails as one member rather than as the archive.
        let file = write(&[("odd.bin", b"0123456789"), ("fine.txt", b"ok")]);
        let mut listing = listing_of(file.path());

        let odd = listing
            .members
            .iter()
            .position(|m| m.path == "odd.bin")
            .unwrap();
        assert_eq!(listing.members[odd].size, 10);
        listing.members[odd].readable = false;

        let mut sink = Collect::default();
        let failures = Zip::open(file.path(), Use::Read)
            .unwrap()
            .extract(
                &listing,
                &(0..listing.members.len()).collect(),
                &mut sink,
                &Progress::default(),
                &Cancel::new(),
            )
            .expect("the pass itself succeeds");

        assert_eq!(failures.len(), 1, "the member fails, the pass does not");
        assert_eq!(failures[0].path, "odd.bin");
        assert_eq!(sink.files.len(), 1, "the other one still comes out");
        assert_eq!(sink.files[0].0, "fine.txt");
    }

    #[test]
    fn a_set_of_members_comes_out_in_one_pass() {
        let file = write(&[
            ("a.txt", b"aaa"),
            ("b.txt", b"bbbb"),
            ("c.txt", b"ccccc"),
            ("d.txt", b"dddddd"),
        ]);
        let listing = listing_of(file.path());

        let wanted: BTreeSet<usize> = listing
            .members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.path != "c.txt")
            .map(|(ix, _)| ix)
            .collect();

        let mut sink = Collect::default();
        let progress = Progress::default();
        let failures = Zip::open(file.path(), Use::Read)
            .unwrap()
            .extract(&listing, &wanted, &mut sink, &progress, &Cancel::new())
            .unwrap();

        assert!(failures.is_empty());
        // Once per wanted member and no more: the whole point of asking for a
        // set instead of asking three times.
        assert_eq!(sink.opens, 3);
        assert_eq!(
            sink.files,
            [
                ("a.txt".to_string(), b"aaa".to_vec()),
                ("b.txt".to_string(), b"bbbb".to_vec()),
                ("d.txt".to_string(), b"dddddd".to_vec()),
            ]
        );
        assert_eq!(progress.files.load(Ordering::Relaxed), 3);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 3 + 4 + 6);
    }

    #[test]
    fn cancelling_stops_a_listing_and_an_extraction() {
        let file = write(&[("a.txt", b"aaa"), ("b.txt", b"bbb")]);
        let cancel = Cancel::new();
        cancel.stop();

        assert!(
            crate::archive::collect(&mut Zip::open(file.path(), Use::List).unwrap(), &cancel)
                .is_err()
        );

        let listing = listing_of(file.path());
        let mut sink = Collect::default();
        assert!(
            Zip::open(file.path(), Use::Read)
                .unwrap()
                .extract(
                    &listing,
                    &BTreeSet::from([0, 1]),
                    &mut sink,
                    &Progress::default(),
                    &cancel,
                )
                .is_err()
        );
        assert!(sink.files.is_empty());
    }

    #[test]
    fn a_stored_member_reads_as_well_as_a_deflated_one() {
        let file = write_with(
            &[("plain.txt", b"stored")],
            ::zip::CompressionMethod::Stored,
        );
        let listing = listing_of(file.path());
        assert!(listing.members[0].readable);

        let mut sink = Collect::default();
        Zip::open(file.path(), Use::Read)
            .unwrap()
            .extract(
                &listing,
                &BTreeSet::from([0]),
                &mut sink,
                &Progress::default(),
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(sink.files[0].1, b"stored");
    }

    /// Not an assertion so much as a design check.
    ///
    /// Listing seeks to each member's local header, so the cost is one seek per
    /// entry rather than one read of the central directory. If that is slow
    /// enough to be felt, the navigation into an archive needs a spinner, or
    /// this needs a cheaper route to the same fields.
    ///
    /// Point `HOJA_TEST_ZIP` at a large archive to run it:
    ///
    /// ```sh
    /// HOJA_TEST_ZIP=~/big.zip cargo test --release -- --ignored --nocapture list_a_large_zip
    /// ```
    #[test]
    #[ignore = "timing measurement, run explicitly with HOJA_TEST_ZIP"]
    fn time_to_list_a_large_zip() {
        let Some(path) = std::env::var_os("HOJA_TEST_ZIP") else {
            eprintln!("set HOJA_TEST_ZIP to an archive");
            return;
        };
        let path = Path::new(&path);

        let started = std::time::Instant::now();
        let listing = listing_of(path);
        let read = started.elapsed();

        let members = listing.members.len();
        let bytes: u64 = listing.members.iter().map(|m| m.size).sum();

        let started = std::time::Instant::now();
        let index = Index::build(listing);
        let arranged = started.elapsed();

        // And the other access pattern, which reads each member straight
        // through rather than seeking between them: the two want opposite
        // things from the buffer, so both are measured before one is chosen.
        let progress = Progress::default();
        let started = std::time::Instant::now();
        let failures = Zip::open(path, Use::Read)
            .unwrap()
            .extract(
                index.listing(),
                &(0..members).collect(),
                &mut Discard,
                &progress,
                &Cancel::new(),
            )
            .unwrap();
        let extracted = started.elapsed();

        eprintln!(
            "{}: {members} members, {bytes} bytes\n  \
             read {read:?}, arranged {arranged:?}, extracted {extracted:?} ({} failed)",
            path.display(),
            failures.len(),
        );
    }

    /// Reads the bytes and drops them, so an extraction can be timed without
    /// also timing a disk.
    struct Discard;

    impl Sink for Discard {
        fn dir(&mut self, _: &Member) -> std::io::Result<()> {
            Ok(())
        }
        fn link(&mut self, _: &Member, _: &Link) -> std::io::Result<()> {
            Ok(())
        }
        fn file(&mut self, _: &Member, body: &mut dyn Read) -> std::io::Result<()> {
            std::io::copy(body, &mut std::io::sink()).map(|_| ())
        }
    }

    #[test]
    fn copying_a_folder_out_lands_it_on_the_disk() {
        // The whole of what copy-out does, through the public entry point:
        // expand the folder, extract in one pass, strip the directory it was
        // selected from, and make the folders the archive never named.
        let file = write(&[
            ("ttf/Inter.ttf", b"inter"),
            ("ttf/sub/Mono.ttf", b"mono"),
            ("LICENSE", b"licence"),
        ]);
        let dest = tempfile::tempdir().unwrap();

        // Selected from the archive's root, so `ttf` arrives as `ttf`.
        let failures = crate::archive::extract(
            file.path(),
            Path::new(""),
            &["ttf".to_string()],
            dest.path(),
            &Progress::default(),
            &Cancel::new(),
        )
        .unwrap();
        assert!(failures.is_empty());

        assert_eq!(
            std::fs::read(dest.path().join("ttf/Inter.ttf")).unwrap(),
            b"inter"
        );
        assert_eq!(
            std::fs::read(dest.path().join("ttf/sub/Mono.ttf")).unwrap(),
            b"mono"
        );
        // Only what was asked for.
        assert!(!dest.path().join("LICENSE").exists());
    }

    #[test]
    fn copying_out_of_a_folder_strips_the_path_to_it() {
        let file = write(&[("ttf/sub/Mono.ttf", b"mono")]);
        let dest = tempfile::tempdir().unwrap();

        // Standing in `ttf` and copying `sub`: what lands is `sub`, not
        // `ttf/sub`. The strip is the whole difference.
        crate::archive::extract(
            file.path(),
            Path::new("ttf"),
            &["ttf/sub".to_string()],
            dest.path(),
            &Progress::default(),
            &Cancel::new(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(dest.path().join("sub/Mono.ttf")).unwrap(),
            b"mono"
        );
        assert!(!dest.path().join("ttf").exists());
    }

    #[test]
    fn a_hostile_name_cannot_write_outside_the_destination() {
        // `Path::join` with an absolute component replaces the path rather than
        // extending it, so a member named `/etc/passwd` would be a way out of
        // the destination entirely. `tidy` refuses the name before the member
        // exists, which is why this ends with nothing extracted rather than
        // with something extracted somewhere else.
        let file = write(&[("../escaped.txt", b"no"), ("fine.txt", b"yes")]);
        // A destination one level inside a directory of its own, so that "did
        // anything escape" is a question about a directory this test owns.
        // Asking it about `/tmp` would be answered by whatever else is in
        // `/tmp`, which is both flaky and, when it does fail, a lie.
        let around = tempfile::tempdir().unwrap();
        let dest = around.path().join("dest");
        std::fs::create_dir(&dest).unwrap();

        crate::archive::extract(
            file.path(),
            Path::new(""),
            &["../escaped.txt".to_string(), "fine.txt".to_string()],
            &dest,
            &Progress::default(),
            &Cancel::new(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(dest.join("fine.txt")).unwrap(),
            b"yes",
            "the safe one still comes out"
        );
        let escaped: Vec<_> = std::fs::read_dir(around.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "dest")
            .collect();
        assert!(escaped.is_empty(), "something got out: {escaped:?}");
    }

    #[test]
    fn a_member_carries_a_modification_time() {
        let file = write(&[("a.txt", b"aaa")]);
        let listing = listing_of(file.path());
        // Whichever of the two sources it came from, a row has to have a date
        // to print in the Modified column.
        assert!(listing.members[0].modified.is_some());
    }
}
