//! The MIME types a file name gives, from the shared MIME database's globs,
//! with an index so a lookup does not try every glob.
//!
//! `xdg_mime::SharedMimeInfo::get_mime_types_from_file_name` compares the
//! name with each of the ~1,700 globs in turn, and makes a lowercase copy of
//! the name for most of them. That measured 37 µs a name in a release build:
//! 3.7 s more to list 100,000 files in a picker that filters by type.
//!
//! This index gives the same answer, by the same rules (a test below holds it
//! to that over the whole system database), in a few hash lookups:
//!
//! - A literal glob (`Makefile`) matches the whole name, in any case.
//! - A simple glob (`*.tar.gz`) is a suffix. It matches the name, or the name
//!   in lowercase unless the glob is case-sensitive (`:cs`).
//! - A full glob (`readme*`, `*.[1-9]`) is matched as a pattern, in the case
//!   it is written. There are about ten.
//!
//! Of the globs that match, those with the highest weight win, and of those,
//! the longest pattern. The types of the globs left are the answer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

pub const UNKNOWN: &str = "application/octet-stream";

struct Entry {
    mime: String,
    weight: i32,
    /// The pattern's length as `xdg_mime` measures it, for the longest-match
    /// rule. A simple glob counts without its `*`.
    len: usize,
    case_sensitive: bool,
}

#[derive(Default)]
pub struct GlobIndex {
    entries: Vec<Entry>,
    /// Lowercase name to entries.
    literal: HashMap<String, Vec<usize>>,
    /// Suffix to entries.
    suffix: HashMap<String, Vec<usize>>,
    /// The longest suffix, so a long name is only tried at its end.
    longest_suffix: usize,
    full: GlobSet,
    /// `full`'s index to entries.
    full_entries: Vec<usize>,
}

impl GlobIndex {
    /// From `mime/globs2` (or the older `mime/globs`) in each XDG data dir,
    /// the user's first, as `xdg_mime::SharedMimeInfo::new` reads them.
    pub fn load() -> GlobIndex {
        let mut dirs = Vec::new();
        let home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")));
        dirs.extend(home);
        match std::env::var_os("XDG_DATA_DIRS") {
            Some(list) => dirs.extend(std::env::split_paths(&list)),
            None => dirs.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from)),
        }
        let mut lines = Vec::new();
        for dir in dirs {
            let mime = dir.join("mime");
            if let Ok(text) = std::fs::read_to_string(mime.join("globs2")) {
                lines.extend(text.lines().filter_map(parse_v2));
            } else if let Ok(text) = std::fs::read_to_string(mime.join("globs")) {
                lines.extend(text.lines().filter_map(parse_v1));
            }
        }
        GlobIndex::build(lines)
    }

    /// From parsed lines, the first of a glob and type pair kept, as a
    /// `HashSet::extend` in `xdg_mime` keeps it.
    fn build(lines: Vec<Line>) -> GlobIndex {
        let mut index = GlobIndex::default();
        let mut seen = HashSet::new();
        let mut full = GlobSetBuilder::new();
        for line in lines {
            if !seen.insert((line.glob.clone(), line.mime.clone())) {
                continue;
            }
            let ix = index.entries.len();
            let kind = kind_of(&line.glob);
            let len = match kind {
                Kind::Simple(suffix) => suffix.len(),
                _ => line.glob.len(),
            };
            match kind {
                Kind::Literal => index
                    .literal
                    .entry(line.glob.to_lowercase())
                    .or_default()
                    .push(ix),
                Kind::Simple(suffix) => {
                    index.longest_suffix = index.longest_suffix.max(suffix.len());
                    index.suffix.entry(suffix.to_owned()).or_default().push(ix);
                }
                Kind::Full => {
                    // `*` crosses `/` as `glob::Pattern` lets it, though a
                    // file name has none.
                    let Ok(glob) = GlobBuilder::new(&line.glob)
                        .literal_separator(false)
                        .backslash_escape(true)
                        .build()
                    else {
                        continue;
                    };
                    full.add(glob);
                    index.full_entries.push(ix);
                }
            }
            index.entries.push(Entry {
                mime: line.mime,
                weight: line.weight,
                len,
                case_sensitive: line.case_sensitive,
            });
        }
        index.full = full.build().unwrap_or_default();
        index
    }

    /// The types `name` gives, or `[UNKNOWN]` when no glob matches.
    pub fn lookup(&self, name: &str) -> Vec<&str> {
        let mut hits: Vec<usize> = Vec::new();
        let lower = name.to_lowercase();

        if let Some(ixs) = self.literal.get(&lower) {
            hits.extend(ixs);
        }
        // Every suffix up to the longest glob's, the empty one included.
        for (text, folded) in [(name, false), (lower.as_str(), true)] {
            if folded && lower == name {
                break;
            }
            let start = text.len().saturating_sub(self.longest_suffix);
            for at in (start..=text.len()).filter(|&at| text.is_char_boundary(at)) {
                let Some(ixs) = self.suffix.get(&text[at..]) else {
                    continue;
                };
                hits.extend(
                    ixs.iter()
                        .filter(|&&ix| !folded || !self.entries[ix].case_sensitive),
                );
            }
        }
        hits.extend(
            self.full
                .matches(name)
                .into_iter()
                .map(|ix| self.full_entries[ix]),
        );

        let Some(weight) = hits.iter().map(|&ix| self.entries[ix].weight).max() else {
            return vec![UNKNOWN];
        };
        hits.retain(|&ix| self.entries[ix].weight == weight);
        let len = hits
            .iter()
            .map(|&ix| self.entries[ix].len)
            .max()
            .unwrap_or(0);
        hits.retain(|&ix| self.entries[ix].len == len);
        hits.sort_unstable();
        hits.dedup();
        hits.into_iter()
            .map(|ix| self.entries[ix].mime.as_str())
            .collect()
    }

    /// Every type a glob gives, and `UNKNOWN`.
    pub fn types(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .map(|entry| entry.mime.as_str())
            .chain([UNKNOWN])
    }
}

struct Line {
    weight: i32,
    mime: String,
    glob: String,
    case_sensitive: bool,
}

/// `weight:type:glob[:flags…]`, as `xdg_mime`'s `Glob::from_v2_string`.
fn parse_v2(line: &str) -> Option<Line> {
    if line.starts_with('#') {
        return None;
    }
    let mut parts = line.split(':');
    let weight = parts.next()?.parse::<i32>().ok().filter(|w| *w >= 0)?;
    let mime = valid_mime(parts.next()?)?;
    let glob = parts.next()?.to_owned();
    let case_sensitive = parts
        .next()
        .is_some_and(|flags| flags.split(',').any(|flag| flag == "cs"));
    Some(Line {
        weight,
        mime,
        glob,
        case_sensitive,
    })
}

/// `type:glob`, weight 50.
fn parse_v1(line: &str) -> Option<Line> {
    if line.starts_with('#') {
        return None;
    }
    let mut parts = line.split(':');
    let mime = valid_mime(parts.next()?)?;
    let glob = parts.next().filter(|glob| !glob.is_empty())?.to_owned();
    if parts.next().is_some() {
        return None;
    }
    Some(Line {
        weight: 50,
        mime,
        glob,
        case_sensitive: false,
    })
}

/// The type as `mime::Mime` would print it, or `None` where it would not
/// parse. `xdg_mime` drops a line whose type does not parse.
fn valid_mime(text: &str) -> Option<String> {
    text.parse::<mime::Mime>().ok().map(|mime| mime.to_string())
}

enum Kind<'a> {
    Literal,
    Simple(&'a str),
    Full,
}

/// As `xdg_mime`'s `determine_type`: a leading `*` and no other special
/// character is a suffix, no special character at all is a literal.
fn kind_of(glob: &str) -> Kind<'_> {
    let special = |b: u8| matches!(b, b'\\' | b'[' | b'*' | b'?');
    match glob.as_bytes() {
        [b'*', rest @ ..] if !rest.iter().copied().any(special) => Kind::Simple(&glob[1..]),
        bytes if !bytes.iter().copied().any(special) => Kind::Literal,
        _ => Kind::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &str) -> GlobIndex {
        GlobIndex::build(text.lines().filter_map(parse_v2).collect())
    }

    fn sorted(mut types: Vec<&str>) -> Vec<&str> {
        types.sort_unstable();
        types
    }

    #[test]
    fn the_rules_of_the_database() {
        let index = index(
            "# comment\n\
             50:image/png:*.png\n\
             50:application/gzip:*.gz\n\
             50:application/x-compressed-tar:*.tar.gz\n\
             10:text/x-readme:readme*\n\
             50:text/x-makefile:Makefile\n\
             50:text/x-csrc:*.c:cs\n\
             50:text/x-c++src:*.C:cs\n\
             60:text/x-higher:*.both\n\
             50:text/x-lower:*.both\n\
             50:text/x-a:*.same\n\
             50:text/x-b:*.same\n",
        );
        assert_eq!(index.lookup("a.png"), ["image/png"]);
        assert_eq!(index.lookup("A.PNG"), ["image/png"], "a glob is case-blind");
        assert_eq!(
            index.lookup("a.tar.gz"),
            ["application/x-compressed-tar"],
            "the longest wins"
        );
        assert_eq!(index.lookup("a.gz"), ["application/gzip"]);
        assert_eq!(
            index.lookup("MAKEFILE"),
            ["text/x-makefile"],
            "a literal is case-blind"
        );
        assert_eq!(index.lookup("a.c"), ["text/x-csrc"]);
        assert_eq!(index.lookup("a.C"), ["text/x-c++src"], ":cs keeps the case");
        assert_eq!(index.lookup("readme.txt"), ["text/x-readme"]);
        assert_eq!(
            index.lookup("README.txt"),
            [UNKNOWN],
            "a full glob keeps its case"
        );
        assert_eq!(
            index.lookup("x.both"),
            ["text/x-higher"],
            "the heaviest wins"
        );
        assert_eq!(sorted(index.lookup("x.same")), ["text/x-a", "text/x-b"]);
        assert_eq!(index.lookup("plain"), [UNKNOWN]);
        assert_eq!(
            index.lookup("é.PNG"),
            ["image/png"],
            "a name that is not ASCII"
        );
    }

    /// The same answers as `xdg_mime` for a name made from every glob in the
    /// system's database, in its own case and in upper case. Skipped where
    /// there is no database.
    #[test]
    fn agrees_with_xdg_mime_on_the_system_database() {
        let ours = GlobIndex::load();
        if ours.entries.is_empty() {
            return;
        }
        let theirs = xdg_mime::SharedMimeInfo::new();
        let mut names = vec![
            "plain".to_owned(),
            "a.tar.gz".to_owned(),
            "README".to_owned(),
        ];
        let made = ours
            .literal
            .keys()
            .cloned()
            .chain(ours.suffix.keys().map(|suffix| format!("file{suffix}")));
        for name in made {
            names.push(name.to_uppercase());
            names.push(name);
        }
        names.extend(
            [
                "makefile.am",
                "readme.md",
                "x.so.1",
                "a.anim1",
                "x.3",
                "001.vdr",
                "Dockerfile.dev",
            ]
            .map(str::to_owned),
        );
        let mut checked = 0;
        for name in &names {
            let mut want: Vec<String> = theirs
                .get_mime_types_from_file_name(name)
                .iter()
                .map(|m| m.to_string())
                .collect();
            want.sort_unstable();
            want.dedup();
            let got: Vec<String> = sorted(ours.lookup(name))
                .into_iter()
                .map(str::to_owned)
                .collect();
            assert_eq!(got, want, "for {name:?}");
            checked += 1;
        }
        assert!(checked > 1000, "only {checked} names");
    }
}
