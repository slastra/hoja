use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One row in a directory listing.
///
/// Deliberately owns its data rather than borrowing from a `std::fs::DirEntry`, because
/// these are built on a background thread and then handed to the UI thread.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// `None` for directories and for entries whose metadata could not be read.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl DirEntry {
    fn from_std(entry: &std::fs::DirEntry) -> Self {
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

/// Read a directory, unsorted. Blocking — always call this from a background thread.
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
            !dotted || !pane_transfer::is_partial_name(&name.to_string_lossy())
        })
        .map(|e| DirEntry::from_std(&e))
        .collect())
}

/// Order a listing in place.
///
/// With `folders_first`, directories group before files regardless of key or
/// direction — flipping the grouping is nobody's idea of "sort by size,
/// descending"; only the order *within* each group reverses.
pub fn sort_entries(entries: &mut [DirEntry], sort: Sort, folders_first: bool) {
    entries.sort_by(|a, b| {
        let group = if folders_first {
            b.is_dir.cmp(&a.is_dir)
        } else {
            std::cmp::Ordering::Equal
        };
        group.then_with(|| {
            let ordering = match sort.key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::Size => a.size.unwrap_or(0).cmp(&b.size.unwrap_or(0)),
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
    prefix
        .chars()
        .all(|p| name_chars.next().is_some_and(|n| chars_eq_ignore_case(n, p)))
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

/// The byte range of the stem, for pre-selection during rename. Shares its
/// definition with the engine's ` (copy)` insertion point so the two cannot
/// drift apart.
/// The row `delta` away from `cursor` in a listing of `len` rows.
///
/// With no cursor — or one left behind by a listing that has since shrunk — a
/// downward step enters at the top and an upward step at the bottom, which is
/// what every list does when you first reach for the arrow keys. Steps past
/// either end clamp rather than wrap: wrapping a file list turns one keypress
/// too many into a jump across the whole directory.
/// Whether `dest` is a legal destination for a drag carrying `sources`.
///
/// Refuses the two drops that are wrong rather than merely useless:
/// a folder dropped into itself or into its own descendant, which the engine
/// would otherwise try to copy into a tree that grows as it writes, and a drop
/// onto a source itself. A drop back into the directory the files came from is
/// refused too — it is a no-op, and refusing it means no target lights up.
pub fn is_valid_drop(sources: &[PathBuf], dest: &Path) -> bool {
    sources.iter().all(|source| {
        // `starts_with` compares whole components, so `/a/bc` is not inside
        // `/a/b` even though the strings share a prefix.
        !dest.starts_with(source) && source.parent() != Some(dest)
    })
}

/// The closest ancestor of `path` that is still a directory, including `path`
/// itself.
///
/// Used when a pane's directory goes away — deleted elsewhere, or on a volume
/// that was unmounted. Walking up beats sitting on an error, because the error
/// state has no way out except typing a new path. `/` always exists, so the
/// walk terminates.
pub fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .map(Path::to_path_buf)
}

pub fn step_row(len: usize, cursor: Option<usize>, delta: isize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match cursor.filter(|&ix| ix <= last) {
        Some(ix) => ix.saturating_add_signed(delta).min(last),
        None if delta > 0 => 0,
        None => last,
    })
}

pub fn stem_range(name: &str) -> std::ops::Range<usize> {
    0..pane_transfer::stem_end(name)
}

/// Human-readable size for the listing's right-hand column.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Local-time timestamp for the Modified column.
pub fn format_time(time: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = time.into();
    local.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn falling_back_to_an_existing_ancestor() {
        use super::nearest_existing_dir;

        // Read-only probes of paths that certainly do and do not exist.
        assert_eq!(nearest_existing_dir(Path::new("/")), Some(PathBuf::from("/")));
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

    use super::*;

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
            natural_cmp(&format!("a{}", "9".repeat(40)), &format!("a{}", "1".repeat(41))),
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
            for key in [SortKey::Name, SortKey::Size, SortKey::Kind, SortKey::Modified] {
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

        sort_entries(&mut v, Sort { key: SortKey::Name, dir: SortDir::Ascending }, true);
        assert_eq!(names(&v), ["file1.txt", "file2.txt", "file10.txt"]);

        sort_entries(&mut v, Sort { key: SortKey::Name, dir: SortDir::Descending }, true);
        assert_eq!(names(&v), ["file10.txt", "file2.txt", "file1.txt"]);
    }

    #[test]
    fn size_and_time_sort_by_value_not_text() {
        let mut v = vec![
            entry("a", false, 900, 300),
            entry("b", false, 1000, 100),
            entry("c", false, 90, 200),
        ];

        sort_entries(&mut v, Sort { key: SortKey::Size, dir: SortDir::Ascending }, true);
        assert_eq!(names(&v), ["c", "a", "b"]);

        sort_entries(&mut v, Sort { key: SortKey::Modified, dir: SortDir::Descending }, true);
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
        let sort = Sort { key: SortKey::Size, dir: SortDir::Ascending };

        sort_entries(&mut v, sort, true);
        assert_eq!(names(&v), ["a", "b", "c"]);
        // Re-sorting must not shuffle an already-sorted list.
        sort_entries(&mut v, sort, true);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }

    #[test]
    fn hidden_files_are_filtered() {
        let dir = std::env::temp_dir().join(format!("pane-fs-test-{}", std::process::id()));
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
        let mut v = vec![
            entry("zdir", true, 0, 1),
            entry("afile", false, 1, 2),
        ];
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
                name: format!("file{i}.txt"),
                path: PathBuf::from(format!("/tmp/manyfiles/file{i}.txt")),
                is_dir: false,
                size: Some(i as u64 % 9973),
                modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i as u64)),
            })
            .collect();

        for key in [SortKey::Name, SortKey::Size, SortKey::Kind, SortKey::Modified] {
            let mut work = entries.clone();
            let start = std::time::Instant::now();
            sort_entries(&mut work, Sort { key, dir: SortDir::Descending }, true);
            println!("{key:?}: {:?}", start.elapsed());
        }
        entries.clear();
    }
}
