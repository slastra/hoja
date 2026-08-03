//! Reading an archive as though it were a directory.
//!
//! # Why the interface looks like this
//!
//! Zip is the first format here and it is the least representative one. It has
//! a central directory, so listing it is a read of one small region, and every
//! member can be decompressed on its own by seeking to it. A gzipped tarball
//! has neither property: the only way to find out what is in one is to inflate
//! the whole stream, and the only way to reach the nine hundredth member is to
//! inflate the eight hundred and ninety-nine before it.
//!
//! An interface shaped around zip's conveniences would therefore have to be
//! rewritten to add tar, so this one is shaped around tar's constraints from
//! the start. Two consequences run through everything below:
//!
//! - **`extract` takes a set of members and makes one pass.** A method that
//!   took a single member would read perfectly well for zip and would turn
//!   extracting five hundred files out of a `.tar.gz` into five hundred full
//!   decompressions of it.
//! - **Cancellation is an explicit flag rather than dropping a handle.**
//!   Dropping a gpui `Task` discards the answer; it does not interrupt a
//!   blocking read already running underneath it. Inflating two gigabytes to
//!   find out what is inside has to be stoppable by the navigation that made it
//!   pointless.
//!
//! # What is not here
//!
//! Writing. Nothing in this module modifies an archive, which is what makes
//! rename, delete and paste refusals rather than half-implemented features.

pub mod tar;
pub mod zip;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// One thing inside an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Relative, `/` separated, no `..`, no leading `/`, never empty.
    ///
    /// Guaranteed by `tidy`, which is the only thing that builds one, and
    /// relied on by everything downstream: it is what makes joining this onto
    /// a destination directory safe rather than a way to write to `/etc`.
    pub path: String,
    pub is_dir: bool,
    /// Set when this member is a link rather than a thing with bytes.
    ///
    /// Toolchain tarballs are full of them, and a copy-out that quietly turned
    /// every symlink into a missing file would be the sort of wrong that only
    /// shows up much later.
    pub link: Option<Link>,
    /// Uncompressed, which is the size the row shows and the size extracting
    /// it would take on the disk.
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,
    /// Whether this build can decompress it.
    ///
    /// A member compressed with something not built in is still listed, with
    /// its name and its real size. Refusing to show it would be a worse answer
    /// than showing it and saying so when someone asks for it.
    pub readable: bool,
}

/// Where a link points, and which sort of link it is.
///
/// The distinction matters at extraction: a symbolic link is a name and can be
/// made on its own, where a hard link is a second name for a file that has to
/// already be there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Sym(String),
    Hard(String),
}

/// Everything one archive holds.
#[derive(Debug, Default)]
pub struct Listing {
    pub members: Vec<Member>,
    /// How many names the archive holds that are not among them: refused by
    /// `tidy`, or a repeat of one already taken.
    ///
    /// Counted rather than listed. A count is enough to tell someone the view
    /// is not the whole archive, and the names are exactly the ones there is no
    /// safe way to show.
    pub skipped: usize,
}

/// A stop signal a blocking read checks as it goes.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// How far an extraction has got, sampled by the UI rather than pushed to it.
#[derive(Debug, Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub files: AtomicU64,
}

/// One member that could not be extracted, while the rest were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub reason: String,
}

/// Where extracted bytes go.
///
/// Separate from the format so that writing into a directory, hashing, and
/// collecting into a buffer for a test all reuse the same single pass.
pub trait Sink: Send {
    fn dir(&mut self, member: &Member) -> io::Result<()>;
    /// `&mut dyn Read` rather than the bytes themselves: a two gigabyte member
    /// must not be held in memory, and for a tarball this reader is a window
    /// onto the decompressor that is only valid for the length of this call.
    fn file(&mut self, member: &Member, body: &mut dyn Read) -> io::Result<()>;
    fn link(&mut self, member: &Member, link: &Link) -> io::Result<()>;
}

/// One archive format.
pub trait Format: Send {
    /// Every member, handed over as it is found. Blocking.
    ///
    /// A callback rather than a returned `Vec` because a tarball's listing *is*
    /// its whole decompression, which for the largest here is a minute of work,
    /// and a pane that shows the first rows straight away is a different thing
    /// from one that shows nothing for a minute. Zip reads its central
    /// directory and finishes in milliseconds either way, so it simply calls
    /// this once per entry.
    ///
    /// `progress.bytes` counts how far through the archive *file* this has
    /// got, which is the only figure that climbs smoothly. Counting the members
    /// instead looks fine on an archive of ten thousand small files and freezes
    /// solid on one holding thirty of three hundred megabytes, which is exactly
    /// what a CUDA package is.
    ///
    /// Returns how many names were refused or repeated. See `Listing::skipped`.
    ///
    /// Shared rather than borrowed because the counting happens inside the
    /// reader, which for a tarball is boxed and, for xz, sent to a thread of
    /// its own. `extract` takes a plain reference because nothing there
    /// outlives the call.
    fn list(
        &mut self,
        cancel: &Cancel,
        progress: &Arc<Progress>,
        found: &mut dyn FnMut(Member),
    ) -> anyhow::Result<usize>;

    /// Write the members named by `wanted` into `sink`, in one pass.
    ///
    /// `wanted` indexes `listing.members`. A member that cannot be read is a
    /// `Failure` and the pass continues, which is how the transfer engine
    /// treats a file it cannot copy: one unreadable thing does not make the
    /// other nine hundred not worth having.
    ///
    /// Returns `Err` only when the pass itself could not be made, cancellation
    /// included.
    fn extract(
        &mut self,
        listing: &Listing,
        wanted: &BTreeSet<usize>,
        sink: &mut dyn Sink,
        progress: &Progress,
        cancel: &Cancel,
    ) -> anyhow::Result<Vec<Failure>>;
}

/// Whether a row is worth offering as something to step into.
///
/// By name, because the alternative is opening every file in a listing to look
/// at its first four bytes. A name that lies is found out when it is opened,
/// which costs one message rather than one read per row.
///
/// `.zip` and the tar family, and deliberately not the many other things that
/// are zip files underneath: `.jar`, `.apk`, `.docx`, `.xlsx`, `.epub` and
/// `.xpi` are all archives by construction and documents by intent, and
/// stepping into a `.docx` instead of opening it would be a bad surprise.
pub fn is_archive(path: &Path) -> bool {
    is_zip(path) || tar::Codec::of(path).is_some()
}

fn is_zip(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// What an archive is about to be used for.
///
/// Listing and reading want opposite things from a buffered reader, and the
/// difference is measured rather than assumed: see `zip::Zip::open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    List,
    Read,
}

/// Everything an archive holds, in one go.
///
/// For the callers that want the whole answer before doing anything: the cache
/// below, and the tests.
pub fn collect(format: &mut dyn Format, cancel: &Cancel) -> anyhow::Result<Listing> {
    let mut members = Vec::new();
    let progress = Arc::new(Progress::default());
    let skipped = format.list(cancel, &progress, &mut |member| members.push(member))?;
    Ok(Listing { members, skipped })
}

/// Open an archive, choosing the format by the file's name.
pub fn open(path: &Path, using: Use) -> anyhow::Result<Box<dyn Format>> {
    if let Some(codec) = tar::Codec::of(path) {
        // `using` has nothing to say here: a tarball is read straight through
        // whatever it is being read for.
        return Ok(Box::new(tar::Tar::open(path, codec)?));
    }
    if is_zip(path) {
        return Ok(Box::new(zip::Zip::open(path, using)?));
    }
    anyhow::bail!("not an archive hoja can read")
}

/// Make a member's name safe to show and safe to join onto a directory, or
/// refuse it.
///
/// Refused: an absolute path, any `..` component, a `.` component, an embedded
/// NUL, and an empty name. Refused rather than repaired, because a repaired
/// `../../etc/passwd` is still a name somebody chose on purpose, and the
/// repaired version would sit in the listing looking ordinary.
///
/// A backslash is left alone. The zip specification says names are `/`
/// separated, some Windows tools have written `\` anyway, and turning one into
/// a separator would split a legitimate Unix filename into directories. Info-ZIP
/// makes the same choice: on Unix a backslash is a character in a name.
pub fn tidy(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }
    // A Windows drive letter is an absolute path wearing a disguise.
    if trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':' {
        return None;
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        // An empty part is a doubled separator, which names the same place.
        if part.is_empty() {
            continue;
        }
        if part == ".." || part == "." {
            return None;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// One row of one directory of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub is_dir: bool,
    /// For a directory, everything below it, which the index already knows
    /// exactly. This is why nothing walks an archive to total a folder: the
    /// answer was in the listing.
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,
    /// Into `Listing.members`, and `None` for a directory the archive itself
    /// never named.
    pub member: Option<usize>,
}

/// A listing arranged so that a pane can ask what is in one directory of it.
#[derive(Debug)]
pub struct Index {
    listing: Listing,
    /// Keyed by directory, `""` being the archive's root. Sorted, so the rows
    /// of a directory come out in a fixed order whatever order the archive
    /// stored them in.
    children: BTreeMap<String, Vec<Row>>,
}

impl Index {
    /// Arrange a listing, synthesising the directories it does not name.
    ///
    /// Not an optimisation: of twelve real zip files sampled on one machine,
    /// three had no directory entries at all. An archive that names none of its
    /// folders and one that names all of them have to produce the same listing.
    pub fn build(mut listing: Listing) -> Index {
        // Duplicates are dropped rather than shown twice: the selection, the
        // sort and the extraction set all key on a member's path, and two rows
        // that cannot be told apart cannot be acted on separately either.
        let mut seen = BTreeSet::new();
        let before = listing.members.len();
        listing.members.retain(|m| seen.insert(m.path.clone()));
        listing.skipped += before - listing.members.len();

        // Every directory that exists, whether the archive said so or not,
        // mapped to the member that named it where one did.
        let mut dirs: BTreeMap<String, Option<usize>> = BTreeMap::new();
        dirs.insert(String::new(), None);
        for (ix, member) in listing.members.iter().enumerate() {
            if member.is_dir {
                dirs.insert(member.path.clone(), Some(ix));
            }
            for parent in ancestors(&member.path) {
                dirs.entry(parent).or_insert(None);
            }
        }

        // What each directory holds, all the way down, and the newest thing in
        // it. One pass over the files: each one adds its size to every
        // directory above it.
        //
        // The date is for the folders an archive never named, which have no
        // date of their own. A blank Modified column beside a folder that
        // plainly contains something reads as a bug, and the newest thing
        // inside is what a directory's own timestamp would have said anyway.
        let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
        let mut newest: BTreeMap<&str, SystemTime> = BTreeMap::new();
        for member in listing.members.iter().filter(|m| !m.is_dir) {
            let mut at = member.path.as_str();
            loop {
                let parent = at.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
                *totals.entry(parent).or_default() += member.size;
                if let Some(when) = member.modified {
                    newest
                        .entry(parent)
                        .and_modify(|held| *held = (*held).max(when))
                        .or_insert(when);
                }
                if parent.is_empty() {
                    break;
                }
                at = parent;
            }
        }

        let mut children: BTreeMap<String, Vec<Row>> =
            dirs.keys().map(|dir| (dir.clone(), Vec::new())).collect();

        for (dir, named_by) in &dirs {
            if dir.is_empty() {
                continue;
            }
            let (parent, name) = split(dir);
            let member = listing.members.get(named_by.unwrap_or(usize::MAX));
            children.entry(parent.to_string()).or_default().push(Row {
                name: name.to_string(),
                is_dir: true,
                size: totals.get(dir.as_str()).copied().unwrap_or(0),
                modified: member
                    .and_then(|m| m.modified)
                    .or_else(|| newest.get(dir.as_str()).copied()),
                mode: member.and_then(|m| m.mode),
                member: *named_by,
            });
        }

        for (ix, member) in listing.members.iter().enumerate() {
            if member.is_dir {
                continue;
            }
            let (parent, name) = split(&member.path);
            children.entry(parent.to_string()).or_default().push(Row {
                name: name.to_string(),
                is_dir: false,
                size: member.size,
                modified: member.modified,
                mode: member.mode,
                member: Some(ix),
            });
        }

        for rows in children.values_mut() {
            rows.sort_by(|a, b| a.name.cmp(&b.name));
        }

        Index { listing, children }
    }

    /// The rows of one directory, and `None` when there is no such directory.
    pub fn rows(&self, inside: &Path) -> Option<&[Row]> {
        self.children.get(&key(inside)).map(Vec::as_slice)
    }

    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    pub fn skipped(&self) -> usize {
        self.listing.skipped
    }
}

/// A directory inside an archive, as the index keys them: `/` separated and
/// relative, with the root as the empty string.
fn key(inside: &Path) -> String {
    inside
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `a/b/c.txt` into `("a/b", "c.txt")`, with the root as `""`.
fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// Every directory above `path`, nearest last, not including the root.
fn ancestors(path: &str) -> impl Iterator<Item = String> + '_ {
    path.match_indices('/')
        .map(|(at, _)| path[..at].to_string())
}

/// A reader that counts what passes through it and stops when told to.
///
/// Wrapping the body rather than checking between members is what makes a
/// single very large member interruptible: cancelling during a four gigabyte
/// file should not have to wait for the four gigabytes.
struct Counted<'a, R> {
    inner: R,
    progress: &'a Progress,
    cancel: &'a Cancel,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.stopped() {
            return Err(io::Error::other("cancelled"));
        }
        let read = self.inner.read(buf)?;
        self.progress
            .bytes
            .fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

/// A calendar date and time, as a `SystemTime`.
///
/// Read as UTC, which for a zip's own timestamp is a guess: the format stores
/// what a clock in an unrecorded timezone said. Every reading of it is a guess,
/// and this is the one that needs no timezone database and is never wrong by
/// more than the offset. Where an archive carries a real Unix timestamp as well
/// (the extended timestamp field, which anything modern writes) that one is
/// used instead and this is not reached.
fn from_civil(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> SystemTime {
    // Howard Hinnant's days_from_civil: March-based years, so a leap day is
    // the last day of the year and needs no special case.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

/// How many archives to keep arranged.
///
/// Two, because a split can have one archive open on each side, and because
/// navigating between the folders of one tarball must not re-read it. An index
/// of a large archive is megabytes, so this is a small number on purpose.
const CACHED: usize = 2;

/// What an index was built from, so that an archive rewritten on the disk is
/// read again rather than remembered wrongly.
#[derive(PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

static CACHE: parking_lot::Mutex<Vec<(Stamp, Arc<Index>)>> = parking_lot::Mutex::new(Vec::new());

/// The arranged contents of an archive, read if they are not already known.
///
/// Blocking. Deliberately does not hold the lock while reading: two panes
/// entering the same large tarball at once would otherwise serialise, and the
/// cost of both reading it is one wasted read rather than a stalled window.
pub fn index(path: &Path, cancel: &Cancel) -> anyhow::Result<Arc<Index>> {
    if let Some(hit) = cached(path) {
        return Ok(hit);
    }
    let index = Index::build(collect(open(path, Use::List)?.as_mut(), cancel)?);
    Ok(remember(path, index))
}

/// What is remembered about `path`, if it is still what is on the disk.
fn cached(path: &Path) -> Option<Arc<Index>> {
    let stamp = stamp(path)?;
    CACHE
        .lock()
        .iter()
        .find(|(known, _)| *known == stamp)
        .map(|(_, index)| Arc::clone(index))
}

/// Remember an index, and hand back the shared copy of it.
fn remember(path: &Path, index: Index) -> Arc<Index> {
    let index = Arc::new(index);
    let Some(stamp) = stamp(path) else {
        return index;
    };
    let mut cache = CACHE.lock();
    cache.retain(|(known, _)| known.path != stamp.path);
    cache.push((stamp, Arc::clone(&index)));
    if cache.len() > CACHED {
        cache.remove(0);
    }
    index
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        path: path.to_path_buf(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Forget what is remembered about `path`, because it has changed.
pub fn forget(path: &Path) {
    CACHE.lock().retain(|(known, _)| known.path != path);
}

/// A reading of an archive that is still going on.
///
/// Shaped after `crate::search::Search`, and for the same reason: the answer
/// takes long enough that handing it over in pieces is the difference between a
/// window that looks alive and one that looks hung. A zip finishes in
/// milliseconds and arrives as a single piece; a `.tar.bz2` of a gigabyte takes
/// a minute, and its rows appear in the first few hundred of them.
pub struct Reading {
    members: std::sync::mpsc::Receiver<Member>,
    cancel: Cancel,
    done: Arc<AtomicBool>,
    /// Names that will not be shown, counted as they are refused.
    skipped: Arc<std::sync::atomic::AtomicUsize>,
    /// How far through the archive file the read has got, for the line that
    /// says so.
    progress: Arc<Progress>,
    /// Set when the read failed outright, which is a different thing from
    /// finishing with nothing in it.
    fault: Arc<parking_lot::Mutex<Option<String>>>,
}

impl Reading {
    /// Whatever has arrived since the last call. Never blocks.
    pub fn drain(&self) -> Vec<Member> {
        let mut found = Vec::new();
        while let Ok(member) = self.members.try_recv() {
            found.push(member);
        }
        found
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn skipped(&self) -> usize {
        self.skipped.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.progress.bytes.load(Ordering::Relaxed)
    }

    /// Why the read stopped, when it stopped badly.
    pub fn fault(&self) -> Option<String> {
        self.fault.lock().clone()
    }
}

impl Drop for Reading {
    /// Dropping the handle stops the work, not just the interest in it.
    fn drop(&mut self) {
        self.cancel.stop();
    }
}

/// Start reading an archive, off the UI thread.
///
/// A cached archive is handed over whole and immediately, so entering one for
/// the second time does not look different from entering a directory.
pub fn spawn_read(archive: &Path, cancel: Cancel) -> Reading {
    let (tx, rx) = std::sync::mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let progress = Arc::new(Progress::default());
    let fault: Arc<parking_lot::Mutex<Option<String>>> = Arc::new(parking_lot::Mutex::new(None));

    let reading = Reading {
        members: rx,
        cancel: cancel.clone(),
        done: Arc::clone(&done),
        skipped: Arc::clone(&skipped),
        progress: Arc::clone(&progress),
        fault: Arc::clone(&fault),
    };

    // Already known: hand it over and finish, without a thread.
    if let Some(index) = cached(archive) {
        for member in &index.listing.members {
            let _ = tx.send(member.clone());
        }
        skipped.store(index.skipped(), Ordering::Relaxed);
        done.store(true, Ordering::Relaxed);
        return reading;
    }

    let path = archive.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("hoja-archive".to_string())
        .spawn(move || {
            let mut all = Vec::new();
            let result = open(&path, Use::List).and_then(|mut format| {
                format.list(&cancel, &progress, &mut |member| {
                    all.push(member.clone());
                    // One at a time, and deliberately not in batches. A batch
                    // has to be flushed by something, and the only thing on
                    // this side that could flush one is the next member: a
                    // CUDA package holds about thirty members of a few hundred
                    // megabytes each, so waiting for a batch to fill meant
                    // waiting ten seconds between rows. The reader coalesces on
                    // its own timer, which is where the batching belongs.
                    //
                    // A send error means it let go, which the cancel flag will
                    // also be saying.
                    let _ = tx.send(member);
                })
            });

            match result {
                Ok(refused) => {
                    skipped.store(refused, Ordering::Relaxed);
                    // Remembered only when the read finished: half a listing is
                    // worse than none, because nothing afterwards would know it
                    // was half.
                    if !cancel.stopped() {
                        remember(
                            &path,
                            Index::build(Listing {
                                members: all,
                                skipped: refused,
                            }),
                        );
                    }
                }
                Err(err) => *fault.lock() = Some(err.to_string()),
            }
            // Last, so a reader that sees `done` has already been offered
            // everything there was.
            done.store(true, Ordering::Relaxed);
        });

    if let Err(err) = spawned {
        *reading.fault.lock() = Some(err.to_string());
        reading.done.store(true, Ordering::Relaxed);
    }
    reading
}

/// One directory of an already-arranged archive, as listing rows.
///
/// Split out so that a reading still in progress can build rows from the
/// members it has, which is what makes a tarball's listing appear while it is
/// still being read rather than a minute later.
pub fn rows_in(index: &Index, archive: &Path, inside: &Path) -> Option<Rows> {
    let rows = index.rows(inside)?;
    let shared: Arc<Path> = Arc::from(archive);
    let entries = rows
        .iter()
        .map(|row| {
            let mut entry =
                crate::fs::DirEntry::in_archive(&shared, inside, &row.name, row.is_dir, row.member);
            // A folder's size is exact and already known, which is why no walk
            // is started for one and why its Size column is filled the moment
            // the listing lands rather than a second later.
            entry.size = Some(row.size);
            entry.modified = row.modified;
            entry.mode = row.mode;
            entry
        })
        .collect();

    Some(Rows {
        entries,
        skipped: index.skipped(),
    })
}

/// What one directory of an archive holds, plus what the archive holds that
/// could not be shown at all.
pub struct Rows {
    pub entries: Vec<crate::fs::DirEntry>,
    pub skipped: usize,
}

/// Every member at or below any of `roots`.
///
/// What selecting a folder means. A folder is a prefix rather than a member:
/// three of twelve real zip files name no folders at all, so a folder's
/// contents cannot be found by looking up the folder.
fn members_under(listing: &Listing, roots: &[String]) -> BTreeSet<usize> {
    let mut wanted = BTreeSet::new();
    for (ix, member) in listing.members.iter().enumerate() {
        let under = roots.iter().any(|root| {
            member.path == *root
                // The `/` matters: `ttf` must not take in `ttfx/a`.
                || member.path.starts_with(root) && member.path.as_bytes().get(root.len()) == Some(&b'/')
        });
        if under {
            wanted.insert(ix);
        }
    }
    wanted
}

/// Writes members into a directory on the disk.
///
/// `strip` is the directory they were selected from, removed from the front of
/// each name so that copying `ttf/sub` out of an archive lands `sub` in the
/// destination rather than `ttf/sub`.
struct Extract {
    dest: PathBuf,
    strip: String,
}

impl Extract {
    /// Where a member lands, and `None` when it is not under `strip` at all.
    ///
    /// Both halves are already safe by construction: `tidy` refused every name
    /// with a `..` or a leading `/` in it before the member existed, which is
    /// what keeps this `join` from being the classic way out of a destination
    /// directory.
    fn target(&self, member: &Member) -> Option<PathBuf> {
        let relative = if self.strip.is_empty() {
            member.path.as_str()
        } else {
            member
                .path
                .strip_prefix(&self.strip)?
                .strip_prefix('/')
                .unwrap_or_default()
        };
        (!relative.is_empty()).then(|| self.dest.join(relative))
    }
}

impl Sink for Extract {
    fn dir(&mut self, member: &Member) -> io::Result<()> {
        match self.target(member) {
            Some(at) => std::fs::create_dir_all(at),
            None => Ok(()),
        }
    }

    fn file(&mut self, member: &Member, body: &mut dyn Read) -> io::Result<()> {
        let Some(at) = self.target(member) else {
            return Ok(());
        };
        // The archive may not name the folders its files sit in, so every file
        // makes its own way rather than relying on a directory member having
        // come first.
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&at)?;
        io::copy(body, &mut out)?;
        // What the archive recorded, where it recorded anything. The execute
        // bit is the one that matters: a script that comes out unrunnable is a
        // file that did not survive the trip.
        if let Some(mode) = member.mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = out.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777));
        }
        Ok(())
    }

    /// What `tar -x` does with the two of them.
    ///
    /// A symbolic link is only a name, so it is made as it is found, dangling
    /// or not: an archive is entitled to hold a link to something outside
    /// itself, and repointing it would be inventing a fact. A hard link is a
    /// second name for one file, so it needs the first name to exist, which it
    /// does when the target was extracted too and does not when it was not.
    fn link(&mut self, member: &Member, link: &Link) -> io::Result<()> {
        let Some(at) = self.target(member) else {
            return Ok(());
        };
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match link {
            Link::Sym(target) => std::os::unix::fs::symlink(target, at),
            Link::Hard(target) => {
                // Resolved against the destination, not the archive: the
                // target is a member path, and `tidy` has already refused
                // anything that could point out of it.
                let from = self.dest.join(
                    target
                        .strip_prefix(&self.strip)
                        .unwrap_or(target)
                        .trim_start_matches('/'),
                );
                std::fs::hard_link(from, at)
            }
        }
    }
}

/// Copy things out of an archive onto the disk, in one pass.
///
/// `roots` are member paths as the pane knows them, files and folders alike;
/// `inside` is the directory they were selected from, which is stripped so the
/// destination gets what was selected rather than the path to it. Blocking.
pub fn extract(
    archive: &Path,
    inside: &Path,
    roots: &[String],
    dest: &Path,
    progress: &Progress,
    cancel: &Cancel,
) -> anyhow::Result<Vec<Failure>> {
    // Through the cache, so the listing a pane is already showing is not read
    // a second time to copy out of it.
    let index = index(archive, cancel)?;
    let wanted = members_under(index.listing(), roots);
    if wanted.is_empty() {
        return Ok(Vec::new());
    }

    let mut sink = Extract {
        dest: dest.to_path_buf(),
        strip: key(inside),
    };
    open(archive, Use::Read)?.extract(index.listing(), &wanted, &mut sink, progress, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> Member {
        Member {
            path: path.to_string(),
            is_dir: false,
            link: None,
            size,
            modified: None,
            mode: None,
            readable: true,
        }
    }

    fn dir(path: &str) -> Member {
        Member {
            is_dir: true,
            size: 0,
            ..file(path, 0)
        }
    }

    fn names(index: &Index, inside: &str) -> Vec<String> {
        index
            .rows(Path::new(inside))
            .expect("directory")
            .iter()
            .map(|row| {
                if row.is_dir {
                    format!("{}/", row.name)
                } else {
                    row.name.clone()
                }
            })
            .collect()
    }

    #[test]
    fn a_hostile_name_is_refused_rather_than_repaired() {
        // Every one of these would, joined onto a destination directory, write
        // somewhere nobody asked for. `..` in particular: `Path::join` with an
        // absolute component replaces the path rather than extending it.
        for bad in [
            "../etc/passwd",
            "a/../../etc/passwd",
            "/etc/passwd",
            "//etc/passwd",
            "C:/Windows/system32",
            "..",
            ".",
            "./x",
            "",
            "/",
            "a\0b",
        ] {
            assert_eq!(tidy(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn an_ordinary_name_survives_tidying() {
        assert_eq!(tidy("a/b/c.txt").as_deref(), Some("a/b/c.txt"));
        // A trailing slash is how a zip says "directory", and is not part of
        // the name.
        assert_eq!(tidy("a/b/").as_deref(), Some("a/b"));
        // A doubled separator names the same place as a single one.
        assert_eq!(tidy("a//b.txt").as_deref(), Some("a/b.txt"));
        // A backslash is a character in a name on Unix, not a separator.
        assert_eq!(tidy(r"a\b.txt").as_deref(), Some(r"a\b.txt"));
    }

    #[test]
    fn directories_the_archive_never_named_are_synthesised() {
        // Three of twelve real zip files sampled held no directory entries at
        // all, so this is the ordinary case rather than the odd one.
        let index = Index::build(Listing {
            members: vec![file("ttf/Inter.ttf", 300), file("ttf/sub/Mono.ttf", 200)],
            skipped: 0,
        });

        assert_eq!(names(&index, ""), ["ttf/"]);
        // By name, folders not first: what column the rows are ordered by and
        // whether folders lead is the pane's decision and the person's setting,
        // and it is applied to these afterwards like any other listing. All
        // this owes is a fixed order, whatever order the archive stored them in.
        assert_eq!(names(&index, "ttf"), ["Inter.ttf", "sub/"]);
        assert_eq!(names(&index, "ttf/sub"), ["Mono.ttf"]);
        assert!(index.rows(Path::new("nope")).is_none());
    }

    #[test]
    fn naming_the_directories_gives_the_same_listing() {
        let bare = Index::build(Listing {
            members: vec![file("ttf/Inter.ttf", 300)],
            skipped: 0,
        });
        let named = Index::build(Listing {
            members: vec![dir("ttf"), file("ttf/Inter.ttf", 300)],
            skipped: 0,
        });
        assert_eq!(names(&bare, ""), names(&named, ""));
        assert_eq!(names(&bare, "ttf"), names(&named, "ttf"));
    }

    #[test]
    fn a_folder_knows_its_own_total_without_walking_anything() {
        // The whole reason the size walk is not merely switched off inside an
        // archive: the listing already holds the exact answer.
        let index = Index::build(Listing {
            members: vec![
                file("ttf/Inter.ttf", 300),
                file("ttf/sub/Mono.ttf", 200),
                file("README", 7),
            ],
            skipped: 0,
        });

        let root = index.rows(Path::new("")).unwrap();
        let ttf = root.iter().find(|r| r.name == "ttf").unwrap();
        assert_eq!(ttf.size, 500);

        let sub = index.rows(Path::new("ttf")).unwrap();
        assert_eq!(sub.iter().find(|r| r.name == "sub").unwrap().size, 200);
    }

    #[test]
    fn a_folder_the_archive_never_named_takes_the_newest_date_inside_it() {
        // It has no date of its own, and a blank Modified column beside a
        // folder that plainly holds something reads as a bug.
        let when = |secs| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        let mut old = file("ttf/Old.ttf", 1);
        old.modified = when(1_000);
        let mut new = file("ttf/New.ttf", 1);
        new.modified = when(9_000);

        let index = Index::build(Listing {
            members: vec![old, new],
            skipped: 0,
        });
        let ttf = &index.rows(Path::new("")).unwrap()[0];
        assert_eq!(ttf.name, "ttf");
        assert_eq!(ttf.modified, when(9_000));
    }

    #[test]
    fn a_repeated_name_appears_once_and_is_counted() {
        let index = Index::build(Listing {
            members: vec![file("a.txt", 1), file("a.txt", 2)],
            skipped: 0,
        });
        assert_eq!(names(&index, ""), ["a.txt"]);
        assert_eq!(index.skipped(), 1);
    }

    #[test]
    fn selecting_a_folder_takes_everything_under_it() {
        // A folder in an archive is a prefix, not a thing: an archive that
        // names no folders still has to hand over the whole of one.
        let listing = Listing {
            members: vec![
                file("ttf/Inter.ttf", 1),
                file("ttf/sub/Mono.ttf", 2),
                file("ttfx/Other.ttf", 4),
                file("README", 8),
            ],
            skipped: 0,
        };

        let under = |roots: &[&str]| {
            members_under(
                &listing,
                &roots.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            )
            .iter()
            .map(|ix| listing.members[*ix].path.as_str())
            .collect::<Vec<_>>()
        };

        assert_eq!(under(&["ttf"]), ["ttf/Inter.ttf", "ttf/sub/Mono.ttf"]);
        // The separator is what stops `ttf` swallowing `ttfx`, which a plain
        // `starts_with` would.
        assert_eq!(under(&["ttfx"]), ["ttfx/Other.ttf"]);
        assert_eq!(under(&["README"]), ["README"]);
        assert_eq!(under(&["ttf/sub"]), ["ttf/sub/Mono.ttf"]);
        assert!(under(&["nothing"]).is_empty());
    }

    #[test]
    fn extraction_lands_what_was_selected_and_not_the_path_to_it() {
        // Copying `sub` out of `pack.zip/ttf` puts `sub` in the destination,
        // the way copying a folder anywhere else does. Without the strip it
        // would arrive as `ttf/sub`.
        let sink = Extract {
            dest: PathBuf::from("/dest"),
            strip: "ttf".to_string(),
        };
        assert_eq!(
            sink.target(&file("ttf/sub/Mono.ttf", 0)),
            Some(PathBuf::from("/dest/sub/Mono.ttf"))
        );
        // From the archive's root there is nothing to strip, so selecting
        // `ttf` there and pasting gives `ttf` in the destination, folder and
        // all. Which is what copying a folder does anywhere else.
        let root = Extract {
            dest: PathBuf::from("/dest"),
            strip: String::new(),
        };
        assert_eq!(
            root.target(&file("ttf/Inter.ttf", 0)),
            Some(PathBuf::from("/dest/ttf/Inter.ttf"))
        );
        // The stripped directory itself lands nowhere: it *is* the destination.
        assert_eq!(sink.target(&dir("ttf")), None);
        // And nothing outside it lands at all.
        assert_eq!(sink.target(&file("other/x", 0)), None);
    }

    #[test]
    fn a_civil_date_converts_to_the_instant_it_names() {
        assert_eq!(from_civil(1970, 1, 1, 0, 0, 0), SystemTime::UNIX_EPOCH);
        assert_eq!(
            from_civil(2001, 9, 9, 1, 46, 40),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
        );
        // The zip epoch, which is what an archive with no timestamp reads as.
        assert_eq!(
            from_civil(1980, 1, 1, 0, 0, 0),
            SystemTime::UNIX_EPOCH + Duration::from_secs(315_532_800)
        );
        // A leap day, which the March-based arithmetic exists to get right.
        assert_eq!(
            from_civil(2024, 2, 29, 12, 0, 0),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_208_000)
        );
    }
}
