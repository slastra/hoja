//! Picker mode: `hoja --pick`.
//!
//! Another program asks for a file. Hoja reads the request as JSON on stdin,
//! opens one window, and writes the answer as one line of JSON on stdout:
//!
//! ```text
//! {"uris":["file:///home/u/photo.png"],"filter":0}
//! ```
//!
//! A cancel writes nothing and exits with 1. `hoja-portal` speaks the
//! desktop portal's D-Bus protocol and turns each request into one of these
//! runs, so this file is the whole of what Hoja knows about the portal.
//!
//! The mode is process-wide: a picker process serves one request and exits.
//! So the request and the selected filter are globals, which every pane
//! reads, new ones included, with nothing passed down to them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::fs::DirEntry;

/// What the program that asked wants. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Request {
    pub title: Option<String>,
    /// The label of the accept button. A `_` marks a mnemonic in GTK's
    /// spelling and is removed.
    pub accept_label: Option<String>,
    pub multiple: bool,
    /// Pick a folder instead of files.
    pub directory: bool,
    pub filters: Vec<Filter>,
    /// An index into `filters`.
    pub current_filter: Option<usize>,
    pub current_folder: Option<PathBuf>,
}

/// One entry of the filter menu. A file matches when it matches one glob or
/// one MIME type.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub name: String,
    pub globs: Vec<String>,
    pub mimes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    File,
    Files,
    Folder,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::File => "file",
            Mode::Files => "files",
            Mode::Folder => "folder",
        }
    }
}

impl Request {
    /// The request in `text`. Empty text is the default request.
    pub fn parse(text: &str) -> anyhow::Result<Request> {
        if text.trim().is_empty() {
            return Ok(Request::default());
        }
        Ok(serde_json_lenient::from_str(text)?)
    }

    pub fn mode(&self) -> Mode {
        match (self.directory, self.multiple) {
            (true, _) => Mode::Folder,
            (false, true) => Mode::Files,
            (false, false) => Mode::File,
        }
    }

    pub fn title(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            match self.mode() {
                Mode::File => "Open File",
                Mode::Files => "Open Files",
                Mode::Folder => "Select Folder",
            }
            .to_string()
        })
    }

    pub fn accept_label(&self) -> String {
        match &self.accept_label {
            Some(label) if !label.is_empty() => without_mnemonic(label),
            _ if self.directory => "Select".to_string(),
            _ => "Open".to_string(),
        }
    }

    /// The filter to start with: the one asked for, else the first.
    pub fn first_filter(&self) -> Option<usize> {
        if self.filters.is_empty() {
            return None;
        }
        Some(
            self.current_filter
                .filter(|&ix| ix < self.filters.len())
                .unwrap_or(0),
        )
    }
}

/// `_Open` → `Open`, and `__` → `_`.
fn without_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Which entries a listing keeps while picking.
#[derive(Debug, Default)]
pub struct Matcher {
    /// Folder mode: no files at all.
    folders_only: bool,
    /// `None` keeps every file.
    filter: Option<(GlobSet, Vec<String>)>,
}

impl Matcher {
    pub fn new(filter: Option<&Filter>, folders_only: bool) -> Matcher {
        let filter = filter.map(|filter| {
            let mut globs = GlobSetBuilder::new();
            for glob in &filter.globs {
                // Case-sensitive, as the portal's spec says. A glob that does
                // not parse matches nothing rather than everything.
                if let Ok(glob) = GlobBuilder::new(glob).literal_separator(true).build() {
                    globs.add(glob);
                }
            }
            let globs = globs.build().unwrap_or_else(|_| GlobSet::empty());
            (globs, filter.mimes.clone())
        });
        Matcher {
            folders_only,
            filter,
        }
    }

    /// Whether an entry named `name` stays in the listing. A folder always
    /// does, so the user can still move through folders.
    pub fn keeps(&self, name: &str, is_dir: bool) -> bool {
        if is_dir {
            return true;
        }
        if self.folders_only {
            return false;
        }
        let Some((globs, mimes)) = &self.filter else {
            return true;
        };
        if globs.is_match(name) {
            return true;
        }
        if mimes.is_empty() {
            return false;
        }
        // By name only: the listing runs this for every row, and reading each
        // file to sniff it would make a large folder slow to open.
        let chain = crate::opener::mime_chain_by_name(name);
        mimes
            .iter()
            .any(|pattern| chain.iter().any(|mime| mime_matches(pattern, mime)))
    }
}

/// `image/*` matches any image type, `*/*` and `*` match all.
fn mime_matches(pattern: &str, mime: &str) -> bool {
    match pattern {
        "*" | "*/*" => true,
        _ => match pattern.strip_suffix("/*") {
            Some(top) => mime.split('/').next() == Some(top),
            None => pattern == mime,
        },
    }
}

/// The picker of this process, when it is one.
struct Picker {
    request: Request,
    filter: RwLock<(Option<usize>, Arc<Matcher>)>,
}

static PICKER: OnceLock<Picker> = OnceLock::new();

/// Make this process a picker. Called once, before the window opens.
pub fn start(request: Request) {
    let first = request.first_filter();
    let matcher = Matcher::new(
        first.map(|ix| &request.filters[ix]),
        request.mode() == Mode::Folder,
    );
    let _ = PICKER.set(Picker {
        request,
        filter: RwLock::new((first, Arc::new(matcher))),
    });
}

/// The request, when this process is a picker.
pub fn request() -> Option<&'static Request> {
    PICKER.get().map(|picker| &picker.request)
}

pub fn active() -> bool {
    PICKER.get().is_some()
}

/// Whether the picker takes one entry only: a file, or a folder.
pub fn picks_one() -> bool {
    request().is_some_and(|request| request.mode() != Mode::Files)
}

/// The filter selected now, as an index into the request's filters.
pub fn filter() -> Option<usize> {
    PICKER
        .get()
        .and_then(|picker| picker.filter.read().ok().and_then(|f| f.0))
}

/// Select another filter. The caller re-lists the panes.
pub fn set_filter(ix: usize) {
    let Some(picker) = PICKER.get() else {
        return;
    };
    let Some(filter) = picker.request.filters.get(ix) else {
        return;
    };
    let matcher = Matcher::new(Some(filter), picker.request.mode() == Mode::Folder);
    if let Ok(mut slot) = picker.filter.write() {
        *slot = (Some(ix), Arc::new(matcher));
    }
}

/// The matcher of the picker, or `None` when this is not one. Cloned out so
/// a listing on another thread can hold it.
pub fn matcher() -> Option<Arc<Matcher>> {
    PICKER
        .get()
        .and_then(|picker| picker.filter.read().ok().map(|f| f.1.clone()))
}

/// Drop what the picker's filter hides. Nothing when this is not a picker.
pub fn retain(matcher: Option<&Matcher>, entries: &mut Vec<DirEntry>) {
    if let Some(matcher) = matcher {
        entries.retain(|entry| matcher.keeps(&entry.name, entry.is_dir));
    }
}

/// A selected row, as the accept rules see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// What the accept button would do now.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Answer with these paths.
    Answer(Vec<PathBuf>),
    /// Go into this folder, as GTK does when a folder is selected in a file
    /// picker.
    Enter(PathBuf),
    /// Nothing can be picked here.
    Nothing(Why),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// No file is selected.
    Empty,
    /// The pane shows the inside of an archive, where nothing has a path that
    /// another program could open.
    Archive,
}

/// The accept rules.
///
/// `selected` is the pane's selection, and `cursor` its lead row. `here` is
/// the folder the pane shows, `None` inside an archive.
pub fn accept(
    mode: Mode,
    selected: &[Pick],
    cursor: Option<&Pick>,
    here: Option<&Path>,
) -> Outcome {
    let Some(here) = here else {
        return Outcome::Nothing(Why::Archive);
    };
    if mode == Mode::Folder {
        return match selected {
            [one] if one.is_dir => Outcome::Answer(vec![one.path.clone()]),
            _ => Outcome::Answer(vec![here.to_path_buf()]),
        };
    }
    if let [one] = selected
        && one.is_dir
    {
        return Outcome::Enter(one.path.clone());
    }
    let files: Vec<&Pick> = selected.iter().filter(|pick| !pick.is_dir).collect();
    if files.is_empty() {
        return Outcome::Nothing(Why::Empty);
    }
    if mode == Mode::File {
        let one = cursor
            .filter(|cursor| files.contains(cursor))
            .unwrap_or(files[0]);
        return Outcome::Answer(vec![one.path.clone()]);
    }
    Outcome::Answer(files.into_iter().map(|pick| pick.path.clone()).collect())
}

impl Outcome {
    /// The bar's text: what the accept button would return, or why it can't.
    pub fn describe(&self) -> String {
        let name = |path: &Path| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        };
        match self {
            Outcome::Answer(paths) if paths.len() == 1 => name(&paths[0]),
            Outcome::Answer(paths) => format!("{} files", paths.len()),
            Outcome::Enter(path) => format!("Open {}", name(path)),
            Outcome::Nothing(Why::Empty) => "Select a file".to_string(),
            Outcome::Nothing(Why::Archive) => "Files in an archive cannot be picked".to_string(),
        }
    }
}

#[derive(Serialize)]
struct Response {
    uris: Vec<String>,
    filter: Option<usize>,
}

/// The line written to stdout for a pick.
pub fn response(paths: &[PathBuf], filter: Option<usize>) -> String {
    let response = Response {
        uris: paths
            .iter()
            .map(|path| crate::uri::file_uri(path))
            .collect(),
        filter,
    };
    serde_json_lenient::to_string(&response).expect("a response serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(path: &str, is_dir: bool) -> Pick {
        Pick {
            path: PathBuf::from(path),
            is_dir,
        }
    }

    #[test]
    fn an_empty_request_is_the_default() {
        let request = Request::parse("  \n").unwrap();
        assert_eq!(request.mode(), Mode::File);
        assert_eq!(request.accept_label(), "Open");
        assert_eq!(request.title(), "Open File");
        assert_eq!(request.first_filter(), None);
    }

    #[test]
    fn a_request_is_read_and_its_mnemonic_removed() {
        let request = Request::parse(
            r#"{"accept_label": "_Upload", "multiple": true,
                "filters": [{"name": "A"}, {"name": "B"}], "current_filter": 1}"#,
        )
        .unwrap();
        assert_eq!(request.mode(), Mode::Files);
        assert_eq!(request.accept_label(), "Upload");
        assert_eq!(request.first_filter(), Some(1));
        assert_eq!(without_mnemonic("Save__as"), "Save_as");

        let folder = Request::parse(r#"{"directory": true, "current_filter": 9}"#).unwrap();
        assert_eq!(folder.mode(), Mode::Folder);
        assert_eq!(folder.accept_label(), "Select");
        assert_eq!(folder.first_filter(), None);
    }

    #[test]
    fn a_glob_is_case_sensitive() {
        let filter = Filter {
            name: "PNG".into(),
            globs: vec!["*.png".into()],
            mimes: vec![],
        };
        let matcher = Matcher::new(Some(&filter), false);
        assert!(matcher.keeps("a.png", false));
        assert!(!matcher.keeps("a.PNG", false));
        assert!(!matcher.keeps("a.txt", false));
    }

    #[test]
    fn a_mime_pattern_matches_by_name_and_by_parent() {
        assert!(mime_matches("image/*", "image/png"));
        assert!(!mime_matches("image/*", "text/plain"));
        assert!(mime_matches("*/*", "text/plain"));
        assert!(mime_matches("text/plain", "text/plain"));

        let images = Filter {
            name: "Images".into(),
            globs: vec![],
            mimes: vec!["image/*".into()],
        };
        let matcher = Matcher::new(Some(&images), false);
        assert!(matcher.keeps("x.png", false));
        assert!(!matcher.keeps("x.txt", false));

        // A Rust file is text/rust, a subclass of text/plain.
        let text = Filter {
            name: "Text".into(),
            globs: vec![],
            mimes: vec!["text/plain".into()],
        };
        assert!(Matcher::new(Some(&text), false).keeps("main.rs", false));
    }

    #[test]
    fn a_folder_always_stays_and_folder_mode_hides_files() {
        let none = Filter {
            name: "None".into(),
            globs: vec!["*.nothing".into()],
            mimes: vec![],
        };
        let matcher = Matcher::new(Some(&none), false);
        assert!(matcher.keeps("sub", true));
        assert!(!matcher.keeps("a.png", false));

        let folders = Matcher::new(None, true);
        assert!(folders.keeps("sub", true));
        assert!(!folders.keeps("a.png", false));

        assert!(Matcher::new(None, false).keeps("anything", false));
    }

    #[test]
    fn the_accept_rules() {
        let here = Path::new("/d");
        let (a, b, sub) = (
            pick("/d/a", false),
            pick("/d/b", false),
            pick("/d/sub", true),
        );

        // Inside an archive, nothing.
        assert_eq!(
            accept(Mode::File, std::slice::from_ref(&a), Some(&a), None),
            Outcome::Nothing(Why::Archive)
        );
        // No selection.
        assert_eq!(
            accept(Mode::File, &[], None, Some(here)),
            Outcome::Nothing(Why::Empty)
        );
        // One folder selected opens it.
        assert_eq!(
            accept(
                Mode::Files,
                std::slice::from_ref(&sub),
                Some(&sub),
                Some(here)
            ),
            Outcome::Enter(PathBuf::from("/d/sub"))
        );
        // Several: files only.
        assert_eq!(
            accept(
                Mode::Files,
                &[a.clone(), sub.clone(), b.clone()],
                None,
                Some(here)
            ),
            Outcome::Answer(vec![PathBuf::from("/d/a"), PathBuf::from("/d/b")])
        );
        // One file wanted: the cursor's.
        assert_eq!(
            accept(Mode::File, &[a.clone(), b.clone()], Some(&b), Some(here)),
            Outcome::Answer(vec![PathBuf::from("/d/b")])
        );
        // A folder: the selected one, else where the pane is.
        assert_eq!(
            accept(
                Mode::Folder,
                std::slice::from_ref(&sub),
                Some(&sub),
                Some(here)
            ),
            Outcome::Answer(vec![PathBuf::from("/d/sub")])
        );
        assert_eq!(
            accept(Mode::Folder, &[], None, Some(here)),
            Outcome::Answer(vec![PathBuf::from("/d")])
        );
    }

    #[test]
    fn the_response_is_one_line_of_escaped_uris() {
        let line = response(&[PathBuf::from("/d/a b\nc.png")], Some(0));
        assert_eq!(line, r#"{"uris":["file:///d/a%20b%0Ac.png"],"filter":0}"#);
        assert!(!line.contains('\n'));
    }
}
