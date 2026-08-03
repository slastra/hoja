use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One row in a directory listing.
///
/// Deliberately owns its data rather than borrowing from a `std::fs::DirEntry`, because
/// these are built on a background thread and then handed to the UI thread.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    /// Where to show this entry when it did not come from the listed directory:
    /// a search result, which lives somewhere below it. `name` stays the
    /// entry's own name, because rename, git colouring, type-ahead, and
    /// selection all key on it.
    pub relative: Option<String>,
    pub path: PathBuf,
    pub is_dir: bool,
    /// `None` for directories and for entries whose metadata could not be read.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl DirEntry {
    pub(crate) fn from_std(entry: &std::fs::DirEntry) -> Self {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();

        // `file_type()` uses the cached type from readdir where the OS provides it and
        // does not follow symlinks, so a symlink to a directory reports as a symlink.
        // Fall back to `metadata()` (which does follow) so linked directories still sort
        // and behave as directories.
        let file_type = entry.file_type().ok();
        let is_symlink = file_type.is_some_and(|t| t.is_symlink());
        let metadata = if is_symlink {
            std::fs::metadata(&path).ok()
        } else {
            entry.metadata().ok()
        };

        let is_dir = match &metadata {
            Some(m) => m.is_dir(),
            None => file_type.is_some_and(|t| t.is_dir()),
        };

        Self {
            name,
            relative: None,
            size: metadata.as_ref().filter(|_| !is_dir).map(|m| m.len()),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            path,
            is_dir,
        }
    }

    /// Coarse type label for the Kind column.
    pub fn kind(&self) -> String {
        if self.is_dir {
            return "Folder".to_string();
        }
        match self.path.extension().and_then(|e| e.to_str()) {
            Some(ext) if !ext.is_empty() => ext.to_uppercase(),
            _ => "File".to_string(),
        }
    }
}

/// Which column the listing is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Kind,
    Modified,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortDir {
    Ascending,
    Descending,
}

impl SortDir {
    pub fn toggled(self) -> Self {
        match self {
            SortDir::Ascending => SortDir::Descending,
            SortDir::Descending => SortDir::Ascending,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            dir: SortDir::Ascending,
        }
    }
}

/// Everything about how a pane presents a listing. One value so splits copy it
/// wholesale and a future config file has a single thing to serialize.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewSettings {
    pub sort: Sort,
    pub show_hidden: bool,
    pub folders_first: bool,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            sort: Sort::default(),
            show_hidden: false,
            folders_first: true,
        }
    }
}

/// Read a directory, unsorted. Blocking: always call this from a background thread.
///
/// Unreadable individual entries are skipped rather than failing the whole listing; a
/// directory with one bad inode should still show everything else.
pub fn read_dir(path: &Path, include_hidden: bool) -> anyhow::Result<Vec<DirEntry>> {
    Ok(std::fs::read_dir(path)?
        .filter_map(Result::ok)
        .filter(|e| {
            use std::os::unix::ffi::OsStrExt;
            let name = e.file_name();
            let dotted = name.as_bytes().first() == Some(&b'.');
            if !include_hidden {
                // Partial names are dot-prefixed, so this subsumes them.
                return !dotted;
            }
            // In-progress transfer temps are an implementation detail; a crash
            // can orphan them until the journal (M4) grows a reaper, and either
            // way they should not appear in listings.
            !dotted || !hoja_transfer::is_partial_name(&name.to_string_lossy())
        })
        .map(|e| DirEntry::from_std(&e))
        .collect())
}

/// Order a listing in place.
///
/// With `folders_first`, directories group before files regardless of key or
/// direction: flipping the grouping is nobody's idea of "sort by size,
/// descending"; only the order *within* each group reverses.
pub fn sort_entries(entries: &mut [DirEntry], sort: Sort, folders_first: bool) {
    sort_entries_by(entries, sort, folders_first, |entry| entry.size);
}

/// The same, where some rows carry a size the entry itself does not, a
/// directory's recursive total, which is measured after the listing was built.
pub fn sort_entries_by(
    entries: &mut [DirEntry],
    sort: Sort,
    folders_first: bool,
    size_of: impl Fn(&DirEntry) -> Option<u64>,
) {
    entries.sort_by(|a, b| {
        let group = if folders_first {
            b.is_dir.cmp(&a.is_dir)
        } else {
            std::cmp::Ordering::Equal
        };
        group.then_with(|| {
            let ordering = match sort.key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::Size => size_of(a).unwrap_or(0).cmp(&size_of(b).unwrap_or(0)),
                // Compare the raw extension rather than `kind()`, which would allocate
                // an uppercased String on every one of ~n log n comparisons.
                SortKey::Kind => natural_cmp(extension_of(a), extension_of(b)),
                SortKey::Modified => a.modified.cmp(&b.modified),
            };
            let ordering = match sort.dir {
                SortDir::Ascending => ordering,
                SortDir::Descending => ordering.reverse(),
            };
            // Name as a final tiebreak keeps equal keys in a stable, predictable order
            // instead of shuffling between sorts.
            ordering.then_with(|| natural_cmp(&a.name, &b.name))
        })
    });
}

fn extension_of(entry: &DirEntry) -> &str {
    entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

/// Compare names so that embedded numbers order numerically: `file2` before `file10`.
///
/// Case-insensitive, with a case-sensitive tiebreak so the order stays total and stable.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((ax, ac)), Some((bx, bc))) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let a_num = take_digits(a, ax, &mut ai);
                    let b_num = take_digits(b, bx, &mut bi);

                    // Compare by value: strip leading zeros, then longer digit run wins,
                    // then lexically. This avoids parsing into an integer, which would
                    // overflow on absurdly long runs of digits.
                    let a_trim = a_num.trim_start_matches('0');
                    let b_trim = b_num.trim_start_matches('0');
                    let ord = a_trim
                        .len()
                        .cmp(&b_trim.len())
                        .then_with(|| a_trim.cmp(b_trim));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = ac
                        .to_lowercase()
                        .cmp(bc.to_lowercase())
                        .then_with(|| ac.cmp(&bc));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// Consume the run of digits starting at `start`, returning it as a slice of `s`.
fn take_digits<'a>(
    s: &'a str,
    start: usize,
    iter: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> &'a str {
    let mut end = start;
    while let Some(&(ix, c)) = iter.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        end = ix + c.len_utf8();
        iter.next();
    }
    &s[start..end]
}

/// First entry matching a type-ahead buffer.
///
/// A buffer of one repeated character ("ddd") cycles: the next entry after
/// `current` whose name starts with it, wrapping. Anything else is a
/// case-insensitive prefix match from the top.
///
/// Deliberately allocation-free: this runs per keystroke over the whole
/// listing, and the miss case (an overtyped prefix) scans every entry.
pub fn type_ahead_target(
    entries: &[DirEntry],
    buffer: &str,
    current: Option<usize>,
) -> Option<usize> {
    if entries.is_empty() || buffer.is_empty() {
        return None;
    }
    let first = buffer.chars().next()?;
    let is_cycle = buffer.chars().count() > 1 && buffer.chars().all(|c| c == first);

    if is_cycle {
        let start = current.map(|ix| ix + 1).unwrap_or(0);
        let n = entries.len();
        return (0..n).map(|offset| (start + offset) % n).find(|&ix| {
            entries[ix]
                .name
                .chars()
                .next()
                .is_some_and(|c| chars_eq_ignore_case(c, first))
        });
    }

    entries
        .iter()
        .position(|entry| starts_with_ignore_case(&entry.name, buffer))
}

fn chars_eq_ignore_case(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Case-insensitive prefix test without allocating. The ASCII fast path is
/// hoisted out of the per-entry scan, which is where the win actually comes
/// from.
fn starts_with_ignore_case(name: &str, prefix: &str) -> bool {
    let (nb, pb) = (name.as_bytes(), prefix.as_bytes());
    if nb.len() < pb.len() {
        return false;
    }
    if prefix.is_ascii() && name.is_ascii() {
        return nb[..pb.len()].eq_ignore_ascii_case(pb);
    }
    let mut name_chars = name.chars();
    prefix.chars().all(|p| {
        name_chars
            .next()
            .is_some_and(|n| chars_eq_ignore_case(n, p))
    })
}

/// Why a proposed file name cannot be used, or `None` when it is acceptable.
pub fn name_problem(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Some("the name is empty");
    }
    if name == "." || name == ".." {
        return Some("the name is reserved");
    }
    if name.contains('/') {
        return Some("the name contains a slash");
    }
    if name.contains('\0') {
        return Some("the name contains a NUL byte");
    }
    None
}

/// Whether `dest` is a legal destination for a drag carrying `sources`.
///
/// Refuses the two drops that are wrong rather than merely useless:
/// a folder dropped into itself or into its own descendant, which the engine
/// would otherwise try to copy into a tree that grows as it writes, and a drop
/// onto a source itself. A drop back into the directory the files came from is
/// refused too, it is a no-op, and refusing it means no target lights up.
pub fn is_valid_drop(sources: &[PathBuf], dest: &Path) -> bool {
    sources.iter().all(|source| {
        // `starts_with` compares whole components, so `/a/bc` is not inside
        // `/a/b` even though the strings share a prefix.
        !dest.starts_with(source) && source.parent() != Some(dest)
    })
}

/// Whether `name` matches a filter query.
///
/// Substring, not fuzzy. A filename filter wants to be predictable: fuzzy
/// matching turns "test" into a hit on `the_second_thing.rs` and the result
/// reads as noise. Smart case, as everywhere else, a lower-case query ignores
/// case, and any capital makes it exact.
pub fn matches_filter(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if query.chars().any(char::is_uppercase) {
        name.contains(query)
    } else {
        name.to_lowercase().contains(query)
    }
}

/// The closest ancestor of `path` that is still a directory, including `path`
/// itself.
///
/// Used when a pane's directory goes away: deleted elsewhere, or on a volume
/// that was unmounted. Walking up beats sitting on an error, because the error
/// state has no way out except typing a new path. `/` always exists, so the
/// walk terminates.
pub fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .map(Path::to_path_buf)
}

/// The row `delta` away from `cursor` in a listing of `len` rows.
///
/// With no cursor, or one left behind by a listing that has since shrunk, a
/// downward step enters at the top and an upward step at the bottom, which is
/// what every list does when you first reach for the arrow keys. Steps past
/// either end clamp rather than wrap: wrapping a file list turns one keypress
/// too many into a jump across the whole directory.
pub fn step_row(len: usize, cursor: Option<usize>, delta: isize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match cursor.filter(|&ix| ix <= last) {
        Some(ix) => ix.saturating_add_signed(delta).min(last),
        None if delta > 0 => 0,
        None => last,
    })
}

/// The byte range of the stem, for pre-selection during rename. Shares its
/// definition with the engine's ` (copy)` insertion point so the two cannot
/// drift apart.
pub fn stem_range(name: &str) -> std::ops::Range<usize> {
    0..hoja_transfer::stem_end(name)
}

/// Human-readable size for the listing's right-hand column.
/// "45s", "2m 15s", "1h 20m": how long is left, at a glance.
///
/// Coarser the further out it is, because a remaining hour is not known to the
/// second and printing it that way would claim otherwise.
///
/// Empty when there is no answer, so the caller shows nothing rather than a
/// placeholder standing in for a number.
pub fn format_remaining(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0. {
        return String::new();
    }
    // Two digits throughout, so the figure is the same width at nine seconds
    // as at fifty, and counting down does not shuffle the words after it.
    //
    // Days exist so the hours field cannot grow a third digit. Without them a
    // few hundred gigabytes onto a slow mount produced "460h 20m", eight
    // characters in a cell sized for seven, and because the cell is
    // right-justified the overflow was clipped off the *leading* edge: the
    // strip read "0h 20m left" for a transfer with nineteen days to go.
    //
    // Past ninety-nine days the estimate is not an estimate, so it says so
    // rather than growing the cell to hold a number nobody would believe.
    let s = seconds.round() as u64;
    match s {
        0..=59 => format!("{s:02}s"),
        60..=3599 => format!("{:02}m {:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{:02}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => match s / 86_400 {
            0..=99 => format!("{:02}d {:02}h", s / 86_400, (s % 86_400) / 3600),
            _ => "99d+".to_string(),
        },
    }
}

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// One tenth, always, for anything measured in a unit prefix.
///
/// The precision used to depend on the number: a tenth below 100 and none above
/// it, so a column of sizes was a column of decimal points that appeared and
/// disappeared and never lined up. Showing the tenth even when it is zero costs
/// two characters and makes "15.0 GB" and "150.7 GB" the same shape.
///
/// Plain bytes keep no tenth. There is no such thing as a tenth of a byte.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (value, unit) = scale(bytes as f64, 1);
    format!("{value:.1} {}", UNITS[unit])
}

/// Reduce to a unit, accounting for what rounding will do to it afterwards.
///
/// The loop divides while the value is at or above 1024, but the caller then
/// rounds, and a value just under the boundary rounds up through it: 1048575
/// bytes left the loop as 1023.999 KB and printed "1024.0 KB". The reading was
/// never wrong by much and always wrong in the same way, and sorting by size
/// put "1024.0 KB" directly under "1.0 MB", which looks like a broken sort with
/// nothing on screen to explain it.
///
/// So the promotion test is against the number as it will be *shown*, not as it
/// is held. `places` is the decimals the caller will print.
fn scale(mut value: f64, places: u32) -> (f64, usize) {
    // Where rounding to `places` first reaches 1024: 1023.95 at one decimal,
    // 1023.5 at none.
    let ceiling = 1024.0 - 0.5 / 10f64.powi(places as i32);
    let mut unit = 0;
    while value >= ceiling && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    (value, unit)
}

/// A transfer rate, which wants the opposite of `format_size`.
///
/// A tenth of a megabyte per second is noise: the figure is an estimate over a
/// two-second window and its last digit changes faster than it can be read.
/// Rounded whole, it holds still long enough to mean something.
pub fn format_rate(bytes_per_second: u64) -> String {
    let (value, unit) = scale(bytes_per_second as f64, 0);
    format!("{value:.0} {}", UNITS[unit])
}

/// Local-time timestamp, for the pane footer's single-file line.
pub fn format_time(time: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = time.into();
    local.format("%Y-%m-%d %H:%M").to_string()
}

/// How long ago, for the Modified column.
///
/// "3 hours ago" answers the question the column is usually asked, which is
/// whether a file is the one just written, rather than what o'clock it was
/// written at. The exact stamp is still one selection away: the pane footer
/// prints it in full for a single file.
///
/// Coarse on purpose, and one unit only. A listing is scanned, not read, and
/// "2 days 4 hours ago" takes longer to take in than "2 days ago" while
/// answering the same question.
///
/// `now` is a parameter so the buckets can be tested without waiting for a
/// clock. A stamp in the future is a broken timestamp, from clock skew or an
/// archive that carried one in, and it reads as "just now": there is no honest
/// answer, and this is the one that does not draw the eye.
pub fn format_time_ago(time: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(time) else {
        return "just now".to_string();
    };
    let secs = elapsed.as_secs();

    // Calendar months and years are uneven, so past a month this counts in
    // average ones. The reading is "about five months", and a column that said
    // "5 months ago" for one file and "4 months ago" for another written the
    // same week would be claiming a precision it does not have.
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 2_629_746; // mean Gregorian month
    // Not twelve of those. A mean year is a few hours longer than 365 days, so
    // a file written exactly a year ago fell one bucket short and read
    // "11 months ago", which is the one reading a year old file must not have.
    const YEAR: u64 = 365 * DAY;

    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };

    match secs {
        s if s < MINUTE => "just now".to_string(),
        s if s < HOUR => plural(s / MINUTE, "minute"),
        s if s < DAY => plural(s / HOUR, "hour"),
        s if s < WEEK => plural(s / DAY, "day"),
        s if s < MONTH => plural(s / WEEK, "week"),
        s if s < YEAR => plural(s / MONTH, "month"),
        s => plural(s / YEAR, "year"),
    }
}

/// The selected rows, and a number that changes whenever they do.
///
/// The set is private and these methods are the only way in, because the footer
/// and the Size column both re-derive from `revision` and "remember to bump the
/// counter" is the kind of rule that survives a review and not the edit after
/// it. It was tried the other way first: eight mutation sites were found and
/// bumped, seven more inside `cx.listener` closures were not, and clicking a
/// row silently stopped updating the footer.
///
/// It lives here rather than beside `DirPane` for the only reason that makes it
/// work: privacy in Rust is per module, so a field on the pane would be freely
/// assignable from every line in that file.
#[derive(Default, Debug)]
pub struct Selection {
    rows: BTreeSet<usize>,
    revision: u64,
}

impl Selection {
    /// Changes on every mutation, whether or not the mutation changed anything.
    /// Over-counting costs one rebuild of a line of text; under-counting leaves
    /// the footer describing a selection that has gone.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().copied()
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.rows.contains(&ix)
    }

    pub fn first(&self) -> Option<usize> {
        self.rows.iter().next().copied()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn set(&mut self, rows: BTreeSet<usize>) {
        self.rows = rows;
        self.revision += 1;
    }

    /// The common case: this row and nothing else.
    pub fn only(&mut self, ix: usize) {
        self.set(BTreeSet::from([ix]));
    }

    pub fn clear(&mut self) {
        self.set(BTreeSet::new());
    }

    pub fn insert(&mut self, ix: usize) {
        self.rows.insert(ix);
        self.revision += 1;
    }

    /// Add the row, or take it away if it is already there.
    pub fn toggle(&mut self, ix: usize) {
        if !self.rows.remove(&ix) {
            self.rows.insert(ix);
        }
        self.revision += 1;
    }
}

// ---------------------------------------------------------------------------
// The pane footer's line.
//
// Kept here, as pure functions over a listing, so that what the footer says is
// decided without a window and can be tested without one. `dir_pane` is left
// only the parts that genuinely need a `Context`: owning the walk and drawing
// the row.

/// What the footer has to say about a listing or a selection.
///
/// The split between `known` and `roots` is the whole point. Every size that
/// arrived with the listing is already in `known`; only directories, whose size
/// nothing knows without walking them, end up in `roots`. A selection of
/// plain files therefore leaves `roots` empty, and empty roots is what tells the
/// pane not to start a thread at all.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Segments before the size: `14 items`, or `src · folder`.
    pub lead: Vec<String>,
    /// Segments after it, a lone file's timestamp.
    pub trail: Vec<String>,
    /// Whether a size belongs on the line at all.
    pub sized: bool,
    /// Bytes the file rows already knew.
    pub known: u64,
    /// Indices of the directory rows this covers, which are the rows whose size
    /// has to be walked for. Empty means the total is already final.
    ///
    /// Indices rather than paths: the pane holds the entries, so it can map one
    /// to a path when it starts a walk and back again when a cell asks what its
    /// own row came to.
    pub rows: Vec<usize>,
}

/// The idle line: what this directory holds.
///
/// It totals **exactly the rows it is describing**, so `11 items · 44.9 GB`
/// means those eleven and toggling hidden files moves both numbers together.
/// The alternative, walking the directory itself, makes the size cover
/// entries the count beside it does not, and stops one walk from serving both
/// this line and the Size column, which needs each row separately anyway.
///
/// A directory of only files needs no walk: their sizes came with the listing.
pub fn summarise_dir(entries: &[DirEntry]) -> Summary {
    let items = match entries.len() {
        1 => "1 item".to_string(),
        n => format!("{} items", crate::notifications::count(n as u64)),
    };

    Summary {
        lead: vec![items],
        trail: Vec::new(),
        sized: true,
        known: entries.iter().filter_map(|entry| entry.size).sum(),
        rows: entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_dir)
            .map(|(ix, _)| ix)
            .collect(),
    }
}

/// The selection line: one file, one folder, or a count.
pub fn summarise_selection(entries: &[DirEntry], selected: &Selection) -> Summary {
    let rows = || selected.iter().filter_map(|ix| entries.get(ix));

    let mut summary = Summary {
        sized: true,
        ..Summary::default()
    };
    // One row is worth naming; past that the name of any one of them is noise
    // and only the count and the total earn the width.
    match (selected.len(), rows().next()) {
        // A selection whose rows have all gone is not a selection yet: the
        // listing is mid-rebuild and `restore_selection` is about to speak.
        (0, _) | (_, None) => return Summary::default(),
        (1, Some(entry)) if entry.is_dir => {
            summary.lead = vec![entry.name.clone(), "folder".to_string()];
        }
        (1, Some(entry)) => {
            summary.lead = vec![entry.name.clone()];
            summary.trail = entry.modified.map(format_time).into_iter().collect();
        }
        (n, _) => {
            summary.lead = vec![format!(
                "{} selected",
                crate::notifications::count(n as u64)
            )];
        }
    }

    // The fast path is this loop finding no directory: `rows` stays empty,
    // `known` is the whole total, and the pane has nothing to wait for.
    for ix in selected.iter() {
        let Some(entry) = entries.get(ix) else {
            continue;
        };
        if entry.is_dir {
            summary.rows.push(ix);
        } else {
            summary.known += entry.size.unwrap_or(0);
        }
    }
    summary
}

/// The line as it reads, given how much of the walk has landed.
///
/// The ellipsis goes on the number rather than the line, because the number is
/// the only part still moving, the shape the job strip already uses for a total
/// it does not have yet.
pub fn compose(summary: &Summary, walked: u64, settled: bool) -> String {
    let mut parts = summary.lead.clone();
    if summary.sized {
        let total = summary.known + walked;
        parts.push(match (settled, total) {
            (true, total) => format_size(total),
            // Nothing counted yet. "0 B" would be a claim rather than a
            // placeholder, and a wrong one within two frames.
            (false, 0) => "…".to_string(),
            (false, total) => format!("{}…", format_size(total)),
        });
    }
    parts.extend(summary.trail.iter().cloned());
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection<const N: usize>(rows: [usize; N]) -> Selection {
        let mut selection = Selection::default();
        selection.set(BTreeSet::from(rows));
        selection
    }

    fn row(name: &str, is_dir: bool, size: Option<u64>) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            relative: None,
            path: PathBuf::from("/tmp/fixture").join(name),
            is_dir,
            size,
            modified: None,
        }
    }

    #[test]
    fn a_selection_of_files_needs_no_walk() {
        // The requirement this whole split exists for: files carry their size
        // in the listing, so selecting them starts no thread.
        let entries = vec![
            row("a.bin", false, Some(1000)),
            row("b.bin", false, Some(2000)),
            row("c.bin", false, Some(4000)),
        ];
        let summary = summarise_selection(&entries, &selection([0, 2]));
        assert!(summary.rows.is_empty(), "nothing to walk");
        assert_eq!(summary.known, 5000);
        assert_eq!(compose(&summary, 0, true), "2 selected · 4.9 KB");
    }

    #[test]
    fn a_selected_folder_defers_to_the_walk() {
        let entries = vec![row("src", true, None), row("a.bin", false, Some(1000))];
        let summary = summarise_selection(&entries, &selection([0]));
        assert_eq!(summary.rows, vec![0], "the folder's row");
        assert_eq!(summary.known, 0);
        assert_eq!(compose(&summary, 0, false), "src · folder · …");
        assert_eq!(
            compose(&summary, 4_300_000, false),
            "src · folder · 4.1 MB…"
        );
        assert_eq!(compose(&summary, 4_300_000, true), "src · folder · 4.1 MB");
    }

    #[test]
    fn a_mixed_selection_splits_what_is_known_from_what_is_not() {
        let entries = vec![
            row("src", true, None),
            row("a.bin", false, Some(1000)),
            row("docs", true, None),
        ];
        let summary = summarise_selection(&entries, &selection([0, 1, 2]));
        assert_eq!(summary.known, 1000, "the file only");
        assert_eq!(summary.rows, vec![0, 2], "both folders' rows");
        assert_eq!(compose(&summary, 9000, true), "3 selected · 9.8 KB");
    }

    #[test]
    fn one_file_is_named_and_dated() {
        let mut file = row("Cargo.toml", false, Some(2867));
        file.modified =
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000));
        let summary = summarise_selection(&[file], &selection([0]));
        let line = compose(&summary, 0, true);
        assert!(line.starts_with("Cargo.toml · 2.8 KB · "), "got {line}");
    }

    #[test]
    fn a_selection_of_vanished_rows_says_nothing() {
        // Mid-rebuild the indices outlive the rows for a frame.
        let summary = summarise_selection(&[], &selection([3, 7]));
        assert_eq!(summary, Summary::default());
        assert_eq!(compose(&summary, 0, true), "");
    }

    #[test]
    fn a_flat_directory_is_totalled_without_walking() {
        let entries = vec![
            row("a.bin", false, Some(1000)),
            row("b.bin", false, Some(2000)),
        ];
        let summary = summarise_dir(&entries);
        assert!(
            summary.rows.is_empty(),
            "no subdirectories, so nothing to walk"
        );
        assert_eq!(compose(&summary, 0, true), "2 items · 2.9 KB");
    }

    #[test]
    fn the_directory_line_totals_exactly_the_rows_it_describes() {
        // The rule that lets one walk serve both this line and the Size column:
        // the total is the sum of the rows, so `.git` being hidden takes it out
        // of the size exactly as it takes it out of the count. Toggling hidden
        // files moves both numbers together instead of only one.
        let entries = vec![
            row("a.bin", false, Some(1000)),
            row("src", true, None),
            row("docs", true, None),
        ];
        let summary = summarise_dir(&entries);
        assert_eq!(summary.known, 1000, "the file rows, already known");
        assert_eq!(summary.rows, vec![1, 2], "the folder rows, to be walked");
        assert_eq!(summary.lead, vec!["3 items"]);
        // The identity the merge rests on: what the footer prints is the file
        // rows plus every folder row's own figure.
        assert_eq!(compose(&summary, 5000 + 300, true), "3 items · 6.2 KB");
    }

    #[test]
    fn sorting_by_size_can_be_told_a_size_the_entry_does_not_carry() {
        // What the settle-triggered re-sort needs: folders order by their
        // measured total, not by the zero their `size` field holds.
        let mut entries = vec![
            row("small", true, None),
            row("large", true, None),
            row("a.bin", false, Some(50)),
        ];
        let measured = |entry: &DirEntry| match entry.name.as_str() {
            "small" => Some(10),
            "large" => Some(9_000),
            _ => entry.size,
        };
        sort_entries_by(
            &mut entries,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Descending,
            },
            true,
            measured,
        );
        let order: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            order,
            vec!["large", "small", "a.bin"],
            "folders first, and within them by measured size"
        );

        // And the plain sort still sees folders as sizeless, so nothing that
        // does not opt in changes behaviour.
        sort_entries(
            &mut entries,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Descending,
            },
            true,
        );
        let order: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(order, vec!["large", "small", "a.bin"], "tie broken by name");
    }

    #[test]
    fn item_counts_are_grouped_and_singular_where_they_should_be() {
        let one = summarise_dir(&[row("a", false, Some(1))]);
        assert_eq!(one.lead, vec!["1 item"]);
        let many: Vec<DirEntry> = (0..1234)
            .map(|i| row(&i.to_string(), false, Some(0)))
            .collect();
        let big = summarise_dir(&many);
        assert_eq!(big.lead, vec!["1,234 items"]);
    }

    #[test]
    fn remaining_time_gets_coarser_the_further_out_it_is() {
        use super::format_remaining;
        assert_eq!(format_remaining(0.), "00s");
        assert_eq!(format_remaining(45.4), "45s");
        assert_eq!(format_remaining(59.6), "01m 00s");
        assert_eq!(format_remaining(135.), "02m 15s");
        assert_eq!(format_remaining(3600.), "01h 00m");
        assert_eq!(format_remaining(4800.), "01h 20m");
        assert_eq!(format_remaining(86_400. - 1.), "23h 59m");
        assert_eq!(format_remaining(86_400.), "01d 00h");
        // The case that clipped: 460 hours is nineteen days, not "460h 20m".
        assert_eq!(format_remaining(460. * 3600.), "19d 04h");
        assert_eq!(format_remaining(99. * 86_400.), "99d 00h");
        assert_eq!(format_remaining(100. * 86_400.), "99d+");
        // A rate of zero divides to infinity; that is not a duration.
        assert_eq!(format_remaining(f64::INFINITY), "");
        assert_eq!(format_remaining(f64::NAN), "");
    }

    #[test]
    fn filtering_names() {
        use super::matches_filter;

        assert!(matches_filter("main.rs", ""), "an empty query matches all");
        assert!(matches_filter("dir_pane.rs", "pane"));
        assert!(!matches_filter("main.rs", "pane"));

        // Lower case ignores case; a capital anywhere makes it exact.
        assert!(matches_filter("README.md", "readme"));
        assert!(matches_filter("README.md", "READ"));
        assert!(!matches_filter("readme.md", "README"));
        assert!(matches_filter("MyNotes.txt", "Notes"));
        assert!(!matches_filter("mynotes.txt", "Notes"));

        // Substring, not subsequence: the letters must be adjacent.
        assert!(!matches_filter("the_second_thing.rs", "test"));
    }

    #[test]
    fn falling_back_to_an_existing_ancestor() {
        use super::nearest_existing_dir;

        // Read-only probes of paths that certainly do and do not exist.
        assert_eq!(
            nearest_existing_dir(Path::new("/")),
            Some(PathBuf::from("/"))
        );
        assert_eq!(
            nearest_existing_dir(Path::new("/tmp")),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(
            nearest_existing_dir(Path::new("/tmp/pane-no-such-dir/deeper/still")),
            Some(PathBuf::from("/tmp"))
        );
        // Nothing under the root survives, so the root is the floor.
        assert_eq!(
            nearest_existing_dir(Path::new("/pane-no-such-top-level/x")),
            Some(PathBuf::from("/"))
        );
    }

    #[test]
    fn valid_drops() {
        use super::is_valid_drop;
        let src = vec![PathBuf::from("/a/project")];

        assert!(is_valid_drop(&src, Path::new("/b")));
        assert!(is_valid_drop(&src, Path::new("/a/other")));

        // Into itself, or into its own descendant.
        assert!(!is_valid_drop(&src, Path::new("/a/project")));
        assert!(!is_valid_drop(&src, Path::new("/a/project/src")));
        assert!(!is_valid_drop(&src, Path::new("/a/project/src/deep")));

        // Back where it came from: a no-op.
        assert!(!is_valid_drop(&src, Path::new("/a")));

        // A shared string prefix is not containment.
        assert!(is_valid_drop(&src, Path::new("/a/project-notes")));

        // One bad source spoils the drop, since the job is one unit.
        let many = vec![PathBuf::from("/x/one"), PathBuf::from("/a/project")];
        assert!(!is_valid_drop(&many, Path::new("/a/project/src")));
        assert!(is_valid_drop(&many, Path::new("/somewhere/else")));
    }

    #[test]
    fn stepping_rows() {
        use super::step_row;

        // An empty listing has nowhere to go.
        assert_eq!(step_row(0, None, 1), None);
        assert_eq!(step_row(0, Some(0), -1), None);

        // First press enters from the near end.
        assert_eq!(step_row(5, None, 1), Some(0));
        assert_eq!(step_row(5, None, -1), Some(4));

        assert_eq!(step_row(5, Some(2), 1), Some(3));
        assert_eq!(step_row(5, Some(2), -1), Some(1));

        // Both ends clamp instead of wrapping.
        assert_eq!(step_row(5, Some(4), 1), Some(4));
        assert_eq!(step_row(5, Some(0), -1), Some(0));

        // A cursor left behind by a listing that shrank re-enters from the end.
        assert_eq!(step_row(3, Some(9), 1), Some(0));
        assert_eq!(step_row(3, Some(9), -1), Some(2));
    }

    #[test]
    fn natural_order_sorts_numbers_by_value() {
        let mut names = vec!["file10", "file2", "file1"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["file1", "file2", "file10"]);
    }

    #[test]
    fn natural_order_handles_leading_zeros_and_long_runs() {
        // Leading zeros do not change numeric value, so these compare equal on the
        // digit run and fall through to the whole-string tiebreak, which puts the
        // zero-padded form first. Arbitrary but total and stable, which is what matters.
        assert_eq!(natural_cmp("a007", "a7"), Ordering::Less);
        assert_eq!(natural_cmp("a08", "a9"), Ordering::Less);
        // Would overflow u64 if this parsed into an integer.
        assert_eq!(
            natural_cmp(
                &format!("a{}", "9".repeat(40)),
                &format!("a{}", "1".repeat(41))
            ),
            Ordering::Less
        );
    }

    #[test]
    fn natural_order_is_case_insensitive_with_stable_tiebreak() {
        assert_eq!(natural_cmp("apple", "Banana"), Ordering::Less);
        assert_ne!(natural_cmp("a", "A"), Ordering::Equal);
    }

    fn entry(name: &str, is_dir: bool, size: u64, secs: u64) -> DirEntry {
        DirEntry {
            relative: None,
            name: name.to_string(),
            path: PathBuf::from(format!("/x/{name}")),
            is_dir,
            size: (!is_dir).then_some(size),
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
        }
    }

    fn names(entries: &[DirEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn directories_group_first_in_both_directions() {
        let mut v = vec![
            entry("b.txt", false, 10, 1),
            entry("zdir", true, 0, 2),
            entry("a.txt", false, 20, 3),
            entry("adir", true, 0, 4),
        ];

        for dir in [SortDir::Ascending, SortDir::Descending] {
            for key in [
                SortKey::Name,
                SortKey::Size,
                SortKey::Kind,
                SortKey::Modified,
            ] {
                sort_entries(&mut v, Sort { key, dir }, true);
                assert!(
                    v[0].is_dir && v[1].is_dir && !v[2].is_dir && !v[3].is_dir,
                    "{key:?}/{dir:?} broke directory grouping: {:?}",
                    names(&v)
                );
            }
        }
    }

    #[test]
    fn name_sort_is_natural_and_reversible() {
        let mut v = vec![
            entry("file10.txt", false, 1, 1),
            entry("file2.txt", false, 2, 2),
            entry("file1.txt", false, 3, 3),
        ];

        sort_entries(
            &mut v,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Ascending,
            },
            true,
        );
        assert_eq!(names(&v), ["file1.txt", "file2.txt", "file10.txt"]);

        sort_entries(
            &mut v,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Descending,
            },
            true,
        );
        assert_eq!(names(&v), ["file10.txt", "file2.txt", "file1.txt"]);
    }

    #[test]
    fn size_and_time_sort_by_value_not_text() {
        let mut v = vec![
            entry("a", false, 900, 300),
            entry("b", false, 1000, 100),
            entry("c", false, 90, 200),
        ];

        sort_entries(
            &mut v,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Ascending,
            },
            true,
        );
        assert_eq!(names(&v), ["c", "a", "b"]);

        sort_entries(
            &mut v,
            Sort {
                key: SortKey::Modified,
                dir: SortDir::Descending,
            },
            true,
        );
        assert_eq!(names(&v), ["a", "c", "b"]);
    }

    #[test]
    fn equal_keys_fall_back_to_name_so_order_is_stable() {
        // Every entry has the same size; only the name tiebreak can order them.
        let mut v = vec![
            entry("c", false, 42, 1),
            entry("a", false, 42, 2),
            entry("b", false, 42, 3),
        ];
        let sort = Sort {
            key: SortKey::Size,
            dir: SortDir::Ascending,
        };

        sort_entries(&mut v, sort, true);
        assert_eq!(names(&v), ["a", "b", "c"]);
        // Re-sorting must not shuffle an already-sorted list.
        sort_entries(&mut v, sort, true);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }

    #[test]
    fn hidden_files_are_filtered() {
        let dir = std::env::temp_dir().join(format!("hoja-fs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("visible.txt"), b"v").unwrap();
        std::fs::write(dir.join(".hidden"), b"h").unwrap();
        std::fs::create_dir_all(dir.join(".hidden-dir")).unwrap();

        let without = read_dir(&dir, false).unwrap();
        assert_eq!(
            without.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["visible.txt"]
        );

        let mut with = read_dir(&dir, true).unwrap();
        with.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            with.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec![".hidden", ".hidden-dir", "visible.txt"]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_first_is_optional() {
        let mut v = vec![entry("zdir", true, 0, 1), entry("afile", false, 1, 2)];
        sort_entries(&mut v, Sort::default(), true);
        assert_eq!(names(&v), ["zdir", "afile"]);
        sort_entries(&mut v, Sort::default(), false);
        assert_eq!(names(&v), ["afile", "zdir"]);
    }

    fn named(names: &[&str]) -> Vec<DirEntry> {
        names.iter().map(|n| entry(n, false, 1, 1)).collect()
    }

    #[test]
    fn type_ahead_prefix_is_case_insensitive_and_first_wins() {
        let v = named(&["Documents", "Downloads", "Music", "dotfiles", "notes.txt"]);
        assert_eq!(type_ahead_target(&v, "do", None), Some(0));
        assert_eq!(type_ahead_target(&v, "dow", None), Some(1));
        assert_eq!(type_ahead_target(&v, "n", None), Some(4));
        assert_eq!(type_ahead_target(&v, "zzz", None), None);
    }

    #[test]
    fn type_ahead_repeated_letter_cycles() {
        let v = named(&["Documents", "Downloads", "Music", "dotfiles", "notes.txt"]);
        assert_eq!(type_ahead_target(&v, "d", None), Some(0));
        assert_eq!(type_ahead_target(&v, "dd", Some(0)), Some(1));
        assert_eq!(type_ahead_target(&v, "ddd", Some(1)), Some(3));
        assert_eq!(type_ahead_target(&v, "dddd", Some(3)), Some(0));
    }

    #[test]
    fn type_ahead_handles_non_ascii() {
        let v = named(&["Ärger", "naïve.txt", "zebra"]);
        assert_eq!(type_ahead_target(&v, "ä", None), Some(0));
        assert_eq!(type_ahead_target(&v, "naï", None), Some(1));
        // A prefix longer than the name must not panic or match.
        assert_eq!(type_ahead_target(&v, "zebraaaa", None), None);
    }

    #[test]
    fn type_ahead_empty_inputs_match_nothing() {
        assert_eq!(type_ahead_target(&[], "a", None), None);
        assert_eq!(type_ahead_target(&named(&["a"]), "", None), None);
    }

    #[test]
    fn name_validation() {
        assert!(name_problem("notes.txt").is_none());
        assert!(name_problem(".hidden").is_none());
        assert!(name_problem("with space").is_none());
        assert!(name_problem("").is_some());
        assert!(name_problem("   ").is_some());
        assert!(name_problem(".").is_some());
        assert!(name_problem("..").is_some());
        assert!(name_problem("a/b").is_some());
    }

    #[test]
    fn stem_ranges() {
        assert_eq!(stem_range("archive.tar.gz"), 0..7);
        assert_eq!(stem_range("notes.txt"), 0..5);
        assert_eq!(stem_range("README"), 0..6);
        assert_eq!(stem_range(".bashrc"), 0..7);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1536 * 1024), "1.5 MB");
    }

    #[test]
    fn a_size_keeps_its_tenth_past_a_hundred() {
        // The tenth used to vanish above 100, so a column of sizes had decimal
        // points that came and went and never lined up with each other.
        assert_eq!(format_size(150 * 1024 * 1024), "150.0 MB");
        assert_eq!(format_size(1023 * 1024 * 1024), "1023.0 MB");
    }

    #[test]
    fn a_value_that_rounds_up_to_1024_moves_to_the_next_unit() {
        // The loop divides while the value is at or above 1024, but the format
        // rounds afterwards, so these used to print "1024.0 KB" and "1024 MB":
        // a unit the ladder never names, sorting directly under "1.0 MB".
        assert_eq!(format_size(1_048_575), "1.0 MB");
        assert_eq!(format_size(1_073_741_823), "1.0 GB");
        assert_eq!(format_size(1_099_511_627_775), "1.0 TB");
        assert_eq!(format_rate(1_073_741_800), "1 GB");
        // The window is wider with no decimals, so a rate promotes earlier:
        // 1023.5 KB rounds to 1024 and has to move, 1023.4 does not.
        assert_eq!(format_rate(1_048_100), "1 MB");
        assert_eq!(format_rate(1_048_000), "1023 KB");
        // Just below it, the reading still belongs to the smaller unit.
        assert_eq!(format_size(1_048_524), "1023.9 KB");
    }

    #[test]
    fn a_rate_never_shows_a_tenth() {
        // The opposite rule: this is an estimate over a two-second window, and
        // the last digit of it changes faster than anyone can read it.
        assert_eq!(format_rate(1536 * 1024), "2 MB");
        assert_eq!(format_rate(150 * 1024 * 1024), "150 MB");
        assert_eq!(format_rate(512), "512 B");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Not an assertion so much as a design check: if re-sorting a very large listing
    /// is slow, header clicks must go through the background executor.
    #[test]
    #[ignore = "timing measurement, run explicitly"]
    fn time_sort_of_100k_entries() {
        let mut entries: Vec<DirEntry> = (0..100_000)
            .map(|i| DirEntry {
                relative: None,
                name: format!("file{i}.txt"),
                path: PathBuf::from(format!("/tmp/manyfiles/file{i}.txt")),
                is_dir: false,
                size: Some(i as u64 % 9973),
                modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i as u64)),
            })
            .collect();

        for key in [
            SortKey::Name,
            SortKey::Size,
            SortKey::Kind,
            SortKey::Modified,
        ] {
            let mut work = entries.clone();
            let start = std::time::Instant::now();
            sort_entries(
                &mut work,
                Sort {
                    key,
                    dir: SortDir::Descending,
                },
                true,
            );
            println!("{key:?}: {:?}", start.elapsed());
        }
        entries.clear();
    }
}

#[cfg(test)]
mod time_ago_tests {
    use super::*;
    use std::time::Duration;

    fn ago(secs: u64) -> String {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        format_time_ago(now - Duration::from_secs(secs), now)
    }

    #[test]
    fn the_first_minute_is_just_now() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
    }

    #[test]
    fn each_unit_takes_over_where_the_last_one_ends() {
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(60 * 60 - 1), "59 minutes ago");
        assert_eq!(ago(60 * 60), "1 hour ago");
        assert_eq!(ago(24 * 3600 - 1), "23 hours ago");
        assert_eq!(ago(24 * 3600), "1 day ago");
        assert_eq!(ago(7 * 86400 - 1), "6 days ago");
        assert_eq!(ago(7 * 86400), "1 week ago");
        assert_eq!(ago(365 * 86400), "1 year ago");
    }

    #[test]
    fn one_of_something_is_not_one_somethings() {
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(120), "2 minutes ago");
    }

    #[test]
    fn a_stamp_from_the_future_does_not_panic_or_shout() {
        // `duration_since` errors rather than going negative, and a file dated
        // ahead of the clock is a broken timestamp rather than an event.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        assert_eq!(
            format_time_ago(now + Duration::from_secs(86400), now),
            "just now"
        );
    }

    #[test]
    fn the_longest_reading_is_the_one_the_column_is_sized_for() {
        // `Column::widest` pins the Modified column against this.
        let longest = (0..40u64)
            .map(|weeks| ago(weeks * 7 * 86400 + 59 * 60))
            .chain((0..60).map(|m| ago(m * 60)))
            .max_by_key(String::len)
            .unwrap();
        assert_eq!(longest.chars().count(), 14, "{longest:?}");
    }
}
