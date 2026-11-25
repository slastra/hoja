//! Picker mode: `hoja --pick`.
//!
//! Another program asks for a file. Hoja reads the request as JSON on stdin,
//! opens one window, and writes the answer as one line of JSON on stdout:
//!
//! ```text
//! {"uris":["file:///home/u/photo.png"],"filter":0}
//! ```
//!
//! A save works the same way: the request says `"save": true`, and the
//! answer is the one file to write, which may not exist yet. With `files` as
//! well, it asks for a folder to save those names in, and the answer is each
//! name in the folder picked.
//!
//! A cancel writes nothing and exits with 1. `hoja-portal` speaks the
//! desktop portal's D-Bus protocol and turns each request into one of these
//! runs, so this file is the whole of what Hoja knows about the portal.
//!
//! The mode is process-wide: a picker process serves one request and exits.
//! So the request and the selected filter are globals, which every pane
//! reads, new ones included, with nothing passed down to them.

use std::collections::HashSet;
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
    /// Ask where to save, not for files that exist.
    pub save: bool,
    /// A save: the name to start with.
    pub current_name: Option<String>,
    /// A save of a file that exists already: its folder is where to start,
    /// and its name the name to start with.
    pub current_file: Option<PathBuf>,
    /// A save of these names: the user picks the folder for all of them.
    pub files: Vec<String>,
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
    /// A name to save one file as.
    Save,
    /// A folder to save the request's `files` in.
    SaveFiles,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::File => "file",
            Mode::Files => "files",
            Mode::Folder => "folder",
            Mode::Save => "save",
            Mode::SaveFiles => "save-files",
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
        if self.save {
            return match self.files.is_empty() {
                true => Mode::Save,
                false => Mode::SaveFiles,
            };
        }
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
                Mode::Save => "Save File",
                Mode::SaveFiles => "Save Files",
            }
            .to_string()
        })
    }

    pub fn accept_label(&self) -> String {
        match &self.accept_label {
            Some(label) if !label.is_empty() => without_mnemonic(label),
            _ if self.save => "Save".to_string(),
            _ if self.directory => "Select".to_string(),
            _ => "Open".to_string(),
        }
    }

    /// The folder to start in: the one asked for, else the folder of the file
    /// a save names.
    pub fn start_folder(&self) -> Option<PathBuf> {
        self.current_folder
            .clone()
            .or_else(|| self.current_file.as_ref()?.parent().map(Path::to_path_buf))
    }

    /// The name a save starts with: the one asked for, else the name of the
    /// file it saves over.
    pub fn start_name(&self) -> String {
        self.current_name
            .clone()
            .or_else(|| {
                let name = self.current_file.as_ref()?.file_name()?;
                Some(name.to_string_lossy().into_owned())
            })
            .unwrap_or_default()
    }

    /// Whether the panes show folders and no files.
    pub fn folders_only(&self) -> bool {
        matches!(self.mode(), Mode::Folder | Mode::SaveFiles)
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
    filter: Option<Kept>,
}

/// One filter, made ready for a listing to test every name.
#[derive(Debug)]
struct Kept {
    globs: GlobSet,
    /// Every type in the database that the filter's types take in, with its
    /// ancestors weighed once here. Then a name costs one lookup in
    /// `mime_globs` and a hash, not a walk up the type tree.
    types: HashSet<String>,
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
            let types = if filter.mimes.is_empty() {
                HashSet::new()
            } else {
                crate::opener::name_globs()
                    .types()
                    .filter(|mime| {
                        let chain = crate::opener::with_ancestors(mime.to_string());
                        filter
                            .mimes
                            .iter()
                            .any(|pattern| chain.iter().any(|mime| mime_matches(pattern, mime)))
                    })
                    .map(str::to_owned)
                    .collect()
            };
            Kept { globs, types }
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
        let Some(kept) = &self.filter else {
            return true;
        };
        if kept.globs.is_match(name) {
            return true;
        }
        if kept.types.is_empty() {
            return false;
        }
        // By name only: the listing runs this for every row, and reading each
        // file to sniff it would make a large folder slow to open.
        crate::opener::name_globs()
            .lookup(name)
            .into_iter()
            .any(|mime| kept.types.contains(mime))
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
    let matcher = Matcher::new(first.map(|ix| &request.filters[ix]), request.folders_only());
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
    let matcher = Matcher::new(Some(filter), picker.request.folders_only());
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
    /// Answer with these folders, from Open Folder in a file picker.
    Folders(Vec<PathBuf>),
    /// Save as these paths. `replaces` of them exist already, and the user
    /// is asked before they are written over.
    Save {
        paths: Vec<PathBuf>,
        replaces: usize,
    },
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
    /// The same, said of Open Folder.
    ArchiveFolder,
    /// The same, said of a save.
    ArchiveSave,
    /// A save with no name typed.
    NoName,
    /// A save to a folder that is not there.
    NoFolder,
}

/// The accept rules, for a request to open. A save has its own, in
/// `accept_save` and `accept_save_files`.
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

/// The Open Folder rules, for a request for files. Zed's ctrl-o asks for
/// files only, so this is the way to open a project folder from it. `None`
/// in folder mode, where the accept button already picks folders.
///
/// The folders selected, else the folder the pane shows. Picking one, the
/// cursor's row when it is a folder.
pub fn accept_folder(
    mode: Mode,
    selected: &[Pick],
    cursor: Option<&Pick>,
    here: Option<&Path>,
) -> Option<Outcome> {
    if matches!(mode, Mode::Folder | Mode::Save | Mode::SaveFiles) {
        return None;
    }
    let Some(here) = here else {
        return Some(Outcome::Nothing(Why::ArchiveFolder));
    };
    let folders: Vec<&Pick> = selected.iter().filter(|pick| pick.is_dir).collect();
    let paths = match folders.as_slice() {
        [] => vec![here.to_path_buf()],
        [first, ..] if mode == Mode::File => {
            let one = cursor
                .filter(|cursor| folders.contains(cursor))
                .unwrap_or(first);
            vec![one.path.clone()]
        }
        all => all.iter().map(|pick| pick.path.clone()).collect(),
    };
    Some(Outcome::Folders(paths))
}

/// The save rules, for a request to save one file.
///
/// `name` is what the name field holds. It can be a path too, relative to
/// `here` or from `~`, as in the GTK dialog. A name that is a folder goes into
/// it. `None` for `here` is the inside of an archive.
pub fn accept_save(name: &str, here: Option<&Path>) -> Outcome {
    let Some(here) = here else {
        return Outcome::Nothing(Why::ArchiveSave);
    };
    if name.trim().is_empty() {
        return Outcome::Nothing(Why::NoName);
    }
    let path = lexical(&here.join(from_home(name)));
    if path.is_dir() {
        return Outcome::Enter(path);
    }
    let in_folder = path.parent().is_some_and(Path::is_dir);
    if name.ends_with('/') || !in_folder || path.file_name().is_none() {
        return Outcome::Nothing(Why::NoFolder);
    }
    let replaces = usize::from(path.symlink_metadata().is_ok());
    Outcome::Save {
        paths: vec![path],
        replaces,
    }
}

/// The save rules, for a request to save several names in one folder: the
/// folder selected, else the folder the pane shows.
pub fn accept_save_files(files: &[String], selected: &[Pick], here: Option<&Path>) -> Outcome {
    let Some(here) = here else {
        return Outcome::Nothing(Why::ArchiveSave);
    };
    let folder = match selected {
        [one] if one.is_dir => one.path.as_path(),
        _ => here,
    };
    // The last part of each name only: a name is put in the folder picked,
    // never beside it.
    let paths: Vec<PathBuf> = files
        .iter()
        .filter_map(|name| Path::new(name).file_name())
        .map(|name| folder.join(name))
        .collect();
    if paths.is_empty() {
        return Outcome::Nothing(Why::NoName);
    }
    let replaces = paths
        .iter()
        .filter(|path| path.symlink_metadata().is_ok())
        .count();
    Outcome::Save { paths, replaces }
}

/// `~` and `~/x` from the home folder.
fn from_home(name: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match name.strip_prefix('~') {
        Some("") => home().unwrap_or_else(|| PathBuf::from(name)),
        Some(rest) if rest.starts_with('/') => match home() {
            Some(home) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(name),
        },
        _ => PathBuf::from(name),
    }
}

/// `.` and `..` taken out by the text alone, so the answer is a plain path.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

impl Outcome {
    /// Whether the button this is for would do something.
    pub fn enabled(&self) -> bool {
        !matches!(self, Outcome::Nothing(_))
    }

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
            Outcome::Folders(paths) if paths.len() == 1 => name(&paths[0]),
            Outcome::Folders(paths) => format!("{} folders", paths.len()),
            Outcome::Save { paths, replaces } => {
                let folder = paths[0].parent().map(name).unwrap_or_default();
                match (paths.len(), replaces) {
                    (1, 0) => format!("Saves in {folder}"),
                    (1, _) => format!("Replaces a file in {folder}"),
                    (n, 0) => format!("Saves {n} files in {folder}"),
                    (n, r) => format!("Saves {n} files in {folder}, {r} of them over files there"),
                }
            }
            Outcome::Nothing(Why::Empty) => "Select a file".to_string(),
            Outcome::Nothing(Why::Archive) => "Files in an archive cannot be picked".to_string(),
            Outcome::Nothing(Why::ArchiveFolder) => {
                "Folders in an archive cannot be picked".to_string()
            }
            Outcome::Nothing(Why::ArchiveSave) => "Nothing can be saved in an archive".to_string(),
            Outcome::Nothing(Why::NoName) => "Type a name".to_string(),
            Outcome::Nothing(Why::NoFolder) => "That folder does not exist".to_string(),
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
    fn the_open_folder_rules() {
        let here = Path::new("/d");
        let file = pick("/d/a.txt", false);
        let one = pick("/d/one", true);
        let two = pick("/d/two", true);
        let folders =
            |paths: &[&str]| Some(Outcome::Folders(paths.iter().map(PathBuf::from).collect()));

        // Nothing selected, or only files: the folder the pane shows.
        assert_eq!(
            accept_folder(Mode::File, &[], None, Some(here)),
            folders(&["/d"])
        );
        assert_eq!(
            accept_folder(
                Mode::Files,
                std::slice::from_ref(&file),
                Some(&file),
                Some(here)
            ),
            folders(&["/d"])
        );
        // The folders selected, and not the files beside them.
        let mixed = [file.clone(), one.clone(), two.clone()];
        assert_eq!(
            accept_folder(Mode::Files, &mixed, Some(&file), Some(here)),
            folders(&["/d/one", "/d/two"])
        );
        // One folder wanted: the cursor's when it is a folder, else the first.
        assert_eq!(
            accept_folder(Mode::File, &mixed, Some(&two), Some(here)),
            folders(&["/d/two"])
        );
        assert_eq!(
            accept_folder(Mode::File, &mixed, Some(&file), Some(here)),
            folders(&["/d/one"])
        );
        // In an archive, nothing. In folder mode, no button at all.
        assert_eq!(
            accept_folder(Mode::File, &[], None, None),
            Some(Outcome::Nothing(Why::ArchiveFolder))
        );
        assert_eq!(accept_folder(Mode::Folder, &[], None, Some(here)), None);

        assert_eq!(folders(&["/d/one"]).unwrap().describe(), "one");
        assert_eq!(
            folders(&["/d/one", "/d/two"]).unwrap().describe(),
            "2 folders"
        );
    }

    #[test]
    fn a_save_request_is_read() {
        let save =
            Request::parse(r#"{"save": true, "current_file": "/home/u/Documents/plan.odt"}"#)
                .unwrap();
        assert_eq!(save.mode(), Mode::Save);
        assert_eq!(save.title(), "Save File");
        assert_eq!(save.accept_label(), "Save");
        assert_eq!(save.start_name(), "plan.odt");
        assert_eq!(
            save.start_folder(),
            Some(PathBuf::from("/home/u/Documents"))
        );
        assert!(!save.folders_only());

        // The name and the folder asked for win over the file's.
        let named = Request::parse(
            r#"{"save": true, "current_name": "copy.odt", "current_folder": "/tmp",
                "current_file": "/home/u/plan.odt"}"#,
        )
        .unwrap();
        assert_eq!(named.start_name(), "copy.odt");
        assert_eq!(named.start_folder(), Some(PathBuf::from("/tmp")));

        let files = Request::parse(r#"{"save": true, "files": ["a.txt", "b.txt"]}"#).unwrap();
        assert_eq!(files.mode(), Mode::SaveFiles);
        assert_eq!(files.title(), "Save Files");
        assert!(files.folders_only());
        assert_eq!(
            accept_folder(Mode::Save, &[], None, Some(Path::new("/"))),
            None
        );
    }

    #[test]
    fn the_save_rules() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path();
        std::fs::create_dir(here.join("sub")).unwrap();
        std::fs::write(here.join("old.txt"), "x").unwrap();

        assert_eq!(
            accept_save("a.txt", None),
            Outcome::Nothing(Why::ArchiveSave)
        );
        assert_eq!(accept_save("  ", Some(here)), Outcome::Nothing(Why::NoName));
        assert_eq!(
            accept_save("new.txt", Some(here)),
            Outcome::Save {
                paths: vec![here.join("new.txt")],
                replaces: 0
            }
        );
        assert_eq!(
            accept_save("old.txt", Some(here)),
            Outcome::Save {
                paths: vec![here.join("old.txt")],
                replaces: 1
            }
        );
        // A path in the field: into a folder, out of one, or to one that is
        // not there.
        assert_eq!(
            accept_save("sub/../sub/new.txt", Some(here)),
            Outcome::Save {
                paths: vec![here.join("sub/new.txt")],
                replaces: 0
            }
        );
        assert_eq!(
            accept_save("sub", Some(here)),
            Outcome::Enter(here.join("sub"))
        );
        assert_eq!(
            accept_save("..", Some(&here.join("sub"))),
            Outcome::Enter(here.to_path_buf())
        );
        assert_eq!(
            accept_save("nowhere/new.txt", Some(here)),
            Outcome::Nothing(Why::NoFolder)
        );
        assert_eq!(
            accept_save("new/", Some(here)),
            Outcome::Nothing(Why::NoFolder)
        );
        assert_eq!(
            accept_save("old.txt", Some(here)).describe(),
            format!(
                "Replaces a file in {}",
                here.file_name().unwrap().to_string_lossy()
            )
        );
    }

    #[test]
    fn the_save_files_rules() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path();
        std::fs::create_dir(here.join("sub")).unwrap();
        std::fs::write(here.join("a.txt"), "x").unwrap();
        let files = vec!["a.txt".to_string(), "../b.txt".to_string()];

        // Where the pane is, and the names alone, never a path out of it.
        assert_eq!(
            accept_save_files(&files, &[], Some(here)),
            Outcome::Save {
                paths: vec![here.join("a.txt"), here.join("b.txt")],
                replaces: 1
            }
        );
        // In the one folder selected.
        let sub = Pick {
            path: here.join("sub"),
            is_dir: true,
        };
        assert_eq!(
            accept_save_files(&files, &[sub], Some(here)),
            Outcome::Save {
                paths: vec![here.join("sub/a.txt"), here.join("sub/b.txt")],
                replaces: 0
            }
        );
        assert_eq!(
            accept_save_files(&files, &[], None),
            Outcome::Nothing(Why::ArchiveSave)
        );
    }

    #[test]
    fn the_response_is_one_line_of_escaped_uris() {
        let line = response(&[PathBuf::from("/d/a b\nc.png")], Some(0));
        assert_eq!(line, r#"{"uris":["file:///d/a%20b%0Ac.png"],"filter":0}"#);
        assert!(!line.contains('\n'));
    }
}
