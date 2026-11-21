//! Syntax highlighting for a text preview, as Zed highlights code: Zed's
//! tree-sitter grammars and its own highlight queries, from its `grammars`
//! crate, coloured from the theme's syntax palette.
//!
//! The excerpt is parsed on its own, sixteen lines cut from the top of the
//! file. tree-sitter recovers from what the cut leaves unfinished, so the
//! lines it can read are coloured and the rest stays plain.
//!
//! This side runs with the facts, off the UI thread, and returns capture
//! names such as `function.method`, not colours. The colours are looked up
//! from the theme when the preview is drawn, so a change of theme recolours
//! a preview that is already made.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator as _};

/// Zed's language directories that name file types. The others in its
/// `grammars` crate (`jsdoc`, `regex`, `markdown-inline` and so on) are only
/// reached by injection from inside another language.
const LANGUAGES: &[&str] = &[
    "bash",
    "c",
    "cpp",
    "css",
    "diff",
    "env",
    "gitcommit",
    "go",
    "gomod",
    "gowork",
    "javascript",
    "json",
    "jsonc",
    "markdown",
    "python",
    "rust",
    "tsx",
    "typescript",
    "yaml",
];

/// One line's highlights: byte ranges within the line, and capture names.
pub type LineHighlights = Vec<(Range<usize>, Arc<str>)>;

/// The highlighters made so far, by language directory; `None` for one
/// that could not be made, so it is not tried again.
type Made = HashMap<&'static str, Option<Arc<Highlighter>>>;

struct Highlighter {
    language: Language,
    query: Query,
    names: Vec<Arc<str>>,
}

/// File suffixes to language directories, from each language's config.
fn suffixes() -> &'static HashMap<String, &'static str> {
    static SUFFIXES: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    SUFFIXES.get_or_init(|| {
        let mut map = HashMap::new();
        for &dir in LANGUAGES {
            if grammars::get_file(&format!("{dir}/config.toml")).is_none() {
                continue;
            }
            for suffix in grammars::load_config(dir).matcher.path_suffixes.iter() {
                map.entry(suffix.to_string()).or_insert(dir);
            }
        }
        map
    })
}

/// The language directory for `path`: its whole name first, then each
/// suffix after a dot from the longest, which is how `flake.lock` finds JSON
/// before `lock` could find anything else.
fn language_of(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let map = suffixes();
    if let Some(dir) = map.get(name) {
        return Some(dir);
    }
    name.match_indices('.')
        .find_map(|(at, _)| map.get(&name[at + 1..]).copied())
}

/// The highlighter for a language directory, made on first use. Compiling a
/// query takes some milliseconds, so a language nobody looks at costs nothing.
fn highlighter(dir: &'static str) -> Option<Arc<Highlighter>> {
    static MADE: OnceLock<Mutex<Made>> = OnceLock::new();
    let made = MADE.get_or_init(Default::default);
    if let Some(found) = made.lock().expect("not poisoned").get(dir) {
        return found.clone();
    }
    let built = build(dir).map(Arc::new);
    made.lock()
        .expect("not poisoned")
        .insert(dir, built.clone());
    built
}

fn build(dir: &str) -> Option<Highlighter> {
    let grammar = grammars::load_config(dir).grammar?;
    let language = grammars::native_grammars()
        .into_iter()
        .find(|(name, _)| *name == grammar.as_ref())?
        .1;
    let source = grammars::load_queries(dir).highlights?;
    let query = match Query::new(&language, &source) {
        Ok(query) => query,
        Err(err) => {
            eprintln!("[hoja] highlights for {dir} not usable: {err}");
            return None;
        }
    };
    let names = query
        .capture_names()
        .iter()
        .map(|name| Arc::from(*name))
        .collect();
    Some(Highlighter {
        language,
        query,
        names,
    })
}

/// Highlights for `lines` of the file at `path`, one list per line, or
/// `None` when no grammar knows the file.
pub fn highlight(path: &Path, lines: &[String]) -> Option<Vec<LineHighlights>> {
    let highlighter = highlighter(language_of(path)?)?;
    let text = lines.join("\n");
    let mut parser = Parser::new();
    parser.set_language(&highlighter.language).ok()?;
    let tree = parser.parse(&text, None)?;

    // Which capture each byte is drawn with. A capture inside another wins
    // for its bytes, a narrower node being the more specific. Between two of
    // the same node the later pattern wins: Zed's queries open with the
    // general case, `(identifier) @variable`, and name the special ones
    // after it, so a function's name is a function and not a variable.
    let mut paint: Vec<Option<(u32, usize)>> = vec![None; text.len()];
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(&highlighter.query, tree.root_node(), text.as_bytes());
    while let Some((matched, index)) = captures.next() {
        let capture = matched.captures()[*index];
        let range = capture.node.byte_range();
        let width = range.len();
        for byte in &mut paint[range] {
            if byte.is_none_or(|(_, held)| width <= held) {
                *byte = Some((capture.index, width));
            }
        }
    }

    // Runs of one capture, cut at the line breaks into each line's own byte
    // offsets.
    let mut out: Vec<LineHighlights> = vec![Vec::new(); lines.len()];
    let mut line = 0;
    let mut line_start = 0;
    let mut at = 0;
    while at < text.len() {
        if text.as_bytes()[at] == b'\n' {
            line += 1;
            line_start = at + 1;
            at += 1;
            continue;
        }
        let Some((capture, _)) = paint[at] else {
            at += 1;
            continue;
        };
        let start = at;
        while at < text.len()
            && text.as_bytes()[at] != b'\n'
            && paint[at].map(|(c, _)| c) == Some(capture)
        {
            at += 1;
        }
        let name = highlighter.names[capture as usize].clone();
        out[line].push((start - line_start..at - line_start, name));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_at<'a>(line: &'a LineHighlights, text: &str, word: &str) -> Vec<&'a str> {
        let at = text.find(word).expect("word in line");
        line.iter()
            .filter(|(range, _)| range.contains(&at))
            .map(|(_, name)| name.as_ref())
            .collect()
    }

    #[test]
    fn rust_is_coloured_as_zed_colours_it() {
        let lines = vec![
            "fn main() {".to_string(),
            "    let greeting = \"hello\";".to_string(),
            "}".to_string(),
        ];
        let out = highlight(Path::new("main.rs"), &lines).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(names_at(&out[0], &lines[0], "fn"), ["keyword"]);
        assert!(names_at(&out[0], &lines[0], "main")[0].starts_with("function"));
        assert_eq!(names_at(&out[1], &lines[1], "let"), ["keyword"]);
        assert!(names_at(&out[1], &lines[1], "\"hello\"")[0].starts_with("string"));
        // Every range is inside its line.
        for (line, spans) in lines.iter().zip(&out) {
            assert!(spans.iter().all(|(range, _)| range.end <= line.len()));
        }
    }

    #[test]
    fn files_find_their_language_by_suffix() {
        assert_eq!(language_of(Path::new("a/b/lib.rs")), Some("rust"));
        assert_eq!(language_of(Path::new("flake.lock")), Some("json"));
        assert_eq!(language_of(Path::new("x.tar.json")), Some("json"));
        assert_eq!(language_of(Path::new("notes.txt")), None);
        assert!(highlight(Path::new("notes.txt"), &["plain".into()]).is_none());
    }

    #[test]
    fn a_cut_line_does_not_stop_the_rest() {
        // An unterminated string, as a cut at 100 columns leaves one.
        let lines = vec![
            "let a = \"never closed".to_string(),
            "fn next() {}".to_string(),
        ];
        let out = highlight(Path::new("x.rs"), &lines).unwrap();
        assert!(!out[1].is_empty(), "the next line is still read");
    }
}
