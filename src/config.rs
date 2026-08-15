//! Where hoja keeps things, and what it keeps there.
//!
//! Three directories, split by who writes them:
//!
//! - `~/.config/hoja/`: **you** write this. `settings.json` and `themes/`.
//!   hoja never writes here, so it cannot reformat the file, drop the comments,
//!   or lose an edit made while it was running.
//! - `~/.local/state/hoja/`: **hoja** writes this. Column widths, window size,
//!   and the view settings you last toggled. Things nobody would author by
//!   hand, which is exactly what XDG_STATE_HOME is for. Also `jobs/`, one
//!   small file per transfer while it runs, which is how an interrupted one
//!   is found and offered back at the next start: see `crate::journal`. Its
//!   own directory rather than a key here, because a record has exactly one
//!   writer and wants none of the read-modify-write below.
//! - `~/.local/share/hoja/`, the command palette's recency counts.
//!
//! Settings seed, state remembers. A new install takes its defaults from
//! `settings.json`; toggling hidden files in the menu records the new value in
//! state, so it survives a restart.
//!
//! Two hoja processes share `state.json`, so a save is a read-modify-write
//! under a lock that publishes only the fields *this* instance changed: see
//! `Dirty`. Writing the whole snapshot back let one window revert the other's
//! settings, which is the ordinary two-window case rather than an exotic one.
//!
//! Where both files answer, the one written more recently wins: see `newer`.
//! While hoja is running that is always the settings file, since it is watched
//! and a save applies immediately. While hoja is closed it is whichever the
//! user actually edited last, which is the point: state cannot be allowed to
//! win unconditionally, or one column drag would make the `view` block of
//! `settings.json` inert forever.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, prelude::*};
use serde::{Deserialize, Serialize};

use crate::fs::{Sort, SortDir, SortKey, ViewSettings};

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    base("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
pub fn state_home() -> Option<PathBuf> {
    base("XDG_STATE_HOME", ".local/state")
}

/// The spec says a relative value is invalid and must be ignored, which is why
/// this filters rather than trusting the variable.
fn base(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
}

pub fn settings_file() -> Option<PathBuf> {
    Some(config_home()?.join("hoja/settings.json"))
}

pub fn state_file() -> Option<PathBuf> {
    Some(state_home()?.join("hoja/state.json"))
}

// ---------------------------------------------------------------------------

/// What you write. Every field is optional so a partial file is valid: writing
/// only `{"theme": "…"}` should not reset everything else to a default.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub theme: Option<String>,
    pub view: ViewDefaults,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewDefaults {
    pub sort: Option<SortSetting>,
    pub show_hidden: Option<bool>,
    pub folders_first: Option<bool>,
    /// Which columns to draw, and in what order. See `ColumnsSetting`.
    pub columns: Option<ColumnsSetting>,
}

/// The `columns` block, in either of the two shapes it is written in.
///
/// A list is a layout: exactly these columns, in this order, which is what
/// hoja writes to its state file and what somebody writes when they mean an
/// arrangement.
///
/// ```jsonc
/// "columns": ["permissions", "size", "modified"]
/// ```
///
/// A map is an adjustment: the usual columns in the usual order, with only
/// what it names changed, so turning one column on is one line rather than
/// six.
///
/// ```jsonc
/// "columns": { "permissions": true }
/// ```
///
/// Untagged, and unambiguous: an array can only be the first, an object only
/// the second. The two spellings exist because the shorter one is much the
/// nicer thing to write by hand and cannot express an order, and the longer
/// one is the only honest way to write one down.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ColumnsSetting {
    Order(Vec<String>),
    Flags(std::collections::HashMap<String, bool>),
}

impl ColumnsSetting {
    fn layout(&self) -> crate::dir_pane::ColumnLayout {
        match self {
            Self::Order(names) => crate::dir_pane::ColumnLayout::from_names(names),
            Self::Flags(flags) => crate::dir_pane::ColumnLayout::from_flags(flags),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SortSetting {
    pub key: SortKeyName,
    pub direction: SortDirName,
}

impl Default for SortSetting {
    fn default() -> Self {
        let sort = Sort::default();
        Self {
            key: sort.key.into(),
            direction: sort.dir.into(),
        }
    }
}

/// Spelled out in the file rather than reusing the internal enums, so the
/// on-disk names are a deliberate choice and renaming a variant cannot silently
/// invalidate everyone's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKeyName {
    Name,
    Size,
    Kind,
    Modified,
    Owner,
    Group,
    Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirName {
    Ascending,
    Descending,
}

impl From<SortKey> for SortKeyName {
    fn from(key: SortKey) -> Self {
        match key {
            SortKey::Name => Self::Name,
            SortKey::Size => Self::Size,
            SortKey::Kind => Self::Kind,
            SortKey::Modified => Self::Modified,
            SortKey::Owner => Self::Owner,
            SortKey::Group => Self::Group,
            SortKey::Permissions => Self::Permissions,
        }
    }
}

impl From<SortKeyName> for SortKey {
    fn from(key: SortKeyName) -> Self {
        match key {
            SortKeyName::Name => Self::Name,
            SortKeyName::Size => Self::Size,
            SortKeyName::Kind => Self::Kind,
            SortKeyName::Modified => Self::Modified,
            SortKeyName::Owner => Self::Owner,
            SortKeyName::Group => Self::Group,
            SortKeyName::Permissions => Self::Permissions,
        }
    }
}

impl From<SortDir> for SortDirName {
    fn from(dir: SortDir) -> Self {
        match dir {
            SortDir::Ascending => Self::Ascending,
            SortDir::Descending => Self::Descending,
        }
    }
}

impl From<SortDirName> for SortDir {
    fn from(dir: SortDirName) -> Self {
        match dir {
            SortDirName::Ascending => Self::Ascending,
            SortDirName::Descending => Self::Descending,
        }
    }
}

impl Settings {
    /// Read the file, or return defaults.
    ///
    /// A malformed file says so and is then ignored: refusing to start because
    /// of a stray comma would be a poor trade, and the message is enough to fix
    /// it. `deny_unknown_fields` means a typo is reported rather than silently
    /// doing nothing.
    pub fn load() -> Self {
        let Some(path) = settings_file() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json_lenient::from_str(&text) {
            Ok(settings) => settings,
            Err(err) => {
                eprintln!("[hoja] {}: {err}", path.display());
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// What hoja writes. Column widths and window size have no hand-authored form;
/// the view settings are here because a toggle should survive a restart.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub sort: Option<SortSetting>,
    pub show_hidden: Option<bool>,
    pub folders_first: Option<bool>,
    /// Keyed by column name so reordering or inserting a column cannot shift
    /// everyone's saved widths onto the wrong ones.
    pub column_widths: std::collections::HashMap<String, f32>,
    /// The same keys again, saying which of them are drawn and in what
    /// order. Always written in the list form; the map form is for hand.
    pub columns: Option<ColumnsSetting>,
    pub window: Option<WindowState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
}

/// Which fields this instance has changed since it last wrote.
///
/// Nothing else can tell them apart. A `State` is a full snapshot, taken when
/// hoja started, so writing all of it back publishes stale answers for every
/// field the user never touched, and if another hoja changed one of those in
/// the meantime, the write silently reverts it. Only the fields flagged here
/// are this instance's to publish.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dirty {
    pub sort: bool,
    pub show_hidden: bool,
    pub folders_first: bool,
    pub column_widths: bool,
    pub columns: bool,
    pub window: bool,
}

impl Dirty {
    pub fn any(&self) -> bool {
        *self != Self::default()
    }
}

impl State {
    pub fn load() -> Self {
        let Some(path) = state_file() else {
            return Self::default();
        };
        Self::read(&path)
    }

    fn read(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        // A corrupt state file is not worth a message: nobody wrote it by hand,
        // and the defaults are perfectly usable.
        serde_json_lenient::from_str(&text).unwrap_or_default()
    }

    /// Lay this instance's changed fields over a state read from disk.
    fn overlay(&self, disk: &mut State, dirty: Dirty) {
        if dirty.sort {
            disk.sort = self.sort;
        }
        if dirty.show_hidden {
            disk.show_hidden = self.show_hidden;
        }
        if dirty.folders_first {
            disk.folders_first = self.folders_first;
        }
        if dirty.column_widths {
            disk.column_widths = self.column_widths.clone();
        }
        if dirty.columns {
            disk.columns = self.columns.clone();
        }
        if dirty.window {
            disk.window = self.window;
        }
    }

    /// Write on a background thread. Called whenever a remembered value changes,
    /// which for a column drag is every frame, so the write is coalesced by the
    /// caller rather than throttled here.
    pub fn save(&self, dirty: Dirty, cx: &App) {
        let state = self.clone();
        cx.background_spawn(async move { state.commit(dirty) })
            .detach();
    }

    /// The same write, on the calling thread.
    ///
    /// Closing the window drops the background executor along with everything
    /// else, so the last write, the one carrying whatever was toggled in the
    /// final moments, has to happen inline or not at all.
    pub fn save_now(&self, dirty: Dirty) {
        self.commit(dirty);
    }

    /// Read what is on disk now, lay this instance's changes over it, write it
    /// back: all while holding the lock, so a second hoja doing the same thing
    /// cannot read between our read and our write.
    fn commit(&self, dirty: Dirty) {
        if let Some(path) = state_file() {
            self.commit_at(&path, dirty);
        }
    }

    /// Split out so tests can drive it against a temporary file rather than
    /// reaching for `$XDG_STATE_HOME`, which is process-wide and would race the
    /// other tests.
    fn commit_at(&self, path: &Path, dirty: Dirty) {
        if !dirty.any() {
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }

        let _lock = FileLock::acquire(&parent.join(".state.lock"));
        let mut disk = Self::read(path);
        self.overlay(&mut disk, dirty);
        if let Ok(text) = serde_json_lenient::to_string_pretty(&disk) {
            write_atomically(path, &text);
        }
    }
}

/// An advisory lock over the read-modify-write of the state file.
///
/// A separate file rather than `state.json` itself: the write finishes with a
/// rename, which puts a *new* inode in place, so a lock held on the old one
/// would guard nothing. Advisory locks only bind processes that ask for them,
/// which here is every hoja and nothing else, the file is ours.
///
/// Failing to lock is not a reason to skip the save. The lock narrows a race
/// measured in microseconds; losing a setting outright is worse than losing it
/// to that race.
struct FileLock(#[allow(dead_code)] std::fs::File);

impl FileLock {
    fn acquire(path: &Path) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).ok()?;
        // Released when the file closes, which is when this is dropped.
        Some(Self(file))
    }
}

/// Write through a temporary file in the same directory, then rename over the
/// target.
///
/// `fs::write` truncates first, so a crash or a kill between the truncation and
/// the write leaves an empty or half-written file, and `State::load` treats a
/// file it cannot parse as absent, so the failure is silent and everything
/// remembered is gone. `rename` within one directory is atomic, so a reader
/// sees either the old file or the new one.
fn write_atomically(path: &Path, text: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // The pid keeps two hoja processes from writing the same temporary file and
    // renaming each other's half-written contents into place.
    let temp = parent.join(format!(".state.json.{}", std::process::id()));
    if std::fs::write(&temp, text).is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

/// Which file's answer wins where both files have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    Settings,
    State,
}

fn pick<T>(state: Option<T>, settings: Option<T>, winner: Winner) -> Option<T> {
    match winner {
        Winner::State => state.or(settings),
        Winner::Settings => settings.or(state),
    }
}

/// Whichever file was written more recently.
///
/// State cannot simply win, which is what it used to do: `remember_view` writes
/// all three view fields at once, and a single column drag reaches it, so
/// after one drag, state has a concrete answer for everything and the `view`
/// block of `settings.json` is inert forever. Editing that file with hoja
/// closed would then do nothing at all, silently.
///
/// Modification time is the honest tiebreak, and it is the one the README
/// documents: the more recent answer applies. A missing file loses; two files
/// that are missing or unreadable leave state ahead, which is the startup case
/// where nothing has been configured.
pub fn newer() -> Winner {
    let mtime = |path: Option<PathBuf>| {
        path.and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok())
    };
    match (mtime(settings_file()), mtime(state_file())) {
        (Some(settings), Some(state)) if settings > state => Winner::Settings,
        (Some(_), None) => Winner::Settings,
        _ => Winner::State,
    }
}

/// The view a new pane starts with, from the two files and whichever of them
/// answered last.
pub fn initial_view(settings: &Settings, state: &State, winner: Winner) -> ViewSettings {
    let default = ViewSettings::default();
    let sort = pick(state.sort, settings.view.sort, winner)
        .map(|s| Sort {
            key: s.key.into(),
            dir: s.direction.into(),
        })
        .unwrap_or(default.sort);

    ViewSettings {
        sort,
        show_hidden: pick(state.show_hidden, settings.view.show_hidden, winner)
            .unwrap_or(default.show_hidden),
        folders_first: pick(state.folders_first, settings.view.folders_first, winner)
            .unwrap_or(default.folders_first),
        // Whole block rather than field by field, as the sort is: one file
        // owns the columns or the other does. Which of the two shapes it is
        // written in decides how much it claims, a list being an arrangement
        // and a map an adjustment.
        columns: pick(state.columns.clone(), settings.view.columns.clone(), winner)
            .map(|setting| setting.layout())
            .unwrap_or(default.columns),
    }
}

/// How long to wait before writing state after a change, so dragging a divider
/// writes once rather than once per frame.
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Whether a watch event means something actually changed.
///
/// Load-bearing, and not obvious: on Linux, *reading* a watched file or listing
/// a watched directory emits `Access(Open)`. A watcher that reacts to every
/// event and then reads what it watches therefore wakes itself up forever, the
/// read is indistinguishable from an edit. Every watcher here reads what it
/// watches, so every one of them needs this.
pub fn is_content_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Drain a watcher channel, reporting whether any real change arrived.
pub fn drain_changes(rx: &std::sync::mpsc::Receiver<notify::Result<notify::Event>>) -> bool {
    let mut changed = false;
    while let Ok(event) = rx.try_recv() {
        if event.is_ok_and(|event| is_content_change(&event)) {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_leaves_the_rest_alone() {
        let settings: Settings = serde_json_lenient::from_str(r#"{"theme": "Rosé Pine"}"#).unwrap();
        assert_eq!(settings.theme.as_deref(), Some("Rosé Pine"));
        assert!(settings.view.show_hidden.is_none(), "unset stays unset");

        let view = initial_view(&settings, &State::default(), Winner::State);
        assert_eq!(
            view,
            ViewSettings::default(),
            "and falls back to the default"
        );
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed() {
        let settings: Settings = serde_json_lenient::from_str(
            r#"{
                // The theme, by name.
                "view": { "show_hidden": true, },
            }"#,
        )
        .unwrap();
        assert_eq!(settings.view.show_hidden, Some(true));
    }

    #[test]
    fn a_column_can_be_turned_on_by_hand() {
        // The whole path a hand-written line takes: parsed, merged, and
        // arrived at a pane's view.
        let settings: Settings =
            serde_json_lenient::from_str(r#"{"view": {"columns": {"permissions": true}}}"#)
                .unwrap();
        let view = initial_view(&settings, &State::default(), Winner::Settings);
        assert!(view.columns.get(crate::dir_pane::Column::Permissions));
        assert!(
            view.columns.get(crate::dir_pane::Column::Size),
            "and says nothing about the columns it does not name"
        );
        assert!(!view.columns.get(crate::dir_pane::Column::Owner));
    }

    #[test]
    fn a_list_of_columns_is_an_arrangement() {
        // The other shape the block takes, and the reason it exists: a map
        // cannot say what order to draw them in.
        let settings: Settings = serde_json_lenient::from_str(
            r#"{"view": {"columns": ["permissions", "size", "modified"]}}"#,
        )
        .unwrap();
        assert!(matches!(
            settings.view.columns,
            Some(ColumnsSetting::Order(_))
        ));

        let view = initial_view(&settings, &State::default(), Winner::Settings);
        assert_eq!(view.columns.names(), ["permissions", "size", "modified"]);
        assert!(
            !view.columns.get(crate::dir_pane::Column::Kind),
            "a list draws what it names and nothing else"
        );
    }

    #[test]
    fn the_two_shapes_are_told_apart_by_their_punctuation() {
        // Untagged, so this is serde matching a JSON array against one variant
        // and an object against the other. Worth pinning: the failure mode is
        // a settings file silently parsing as the wrong shape.
        let map: Settings =
            serde_json_lenient::from_str(r#"{"view": {"columns": {"owner": true}}}"#).unwrap();
        assert!(matches!(map.view.columns, Some(ColumnsSetting::Flags(_))));

        let list: Settings =
            serde_json_lenient::from_str(r#"{"view": {"columns": ["owner"]}}"#).unwrap();
        assert!(matches!(list.view.columns, Some(ColumnsSetting::Order(_))));

        // And the two mean different things, which is the point of keeping
        // both: the map leaves the usual columns alone, the list replaces them.
        let map = initial_view(&map, &State::default(), Winner::Settings);
        let list = initial_view(&list, &State::default(), Winner::Settings);
        assert_eq!(map.columns.names(), ["size", "kind", "modified", "owner"]);
        assert_eq!(list.columns.names(), ["owner"]);
    }

    #[test]
    fn one_file_owns_the_column_set() {
        // Whole map rather than field by field, so the two files cannot each
        // contribute half a set and produce one neither of them asked for.
        let settings: Settings = serde_json_lenient::from_str(
            r#"{"view": {"columns": {"owner": true, "size": false}}}"#,
        )
        .unwrap();
        let state = State {
            columns: Some(ColumnsSetting::Flags(std::collections::HashMap::from([(
                "permissions".to_string(),
                true,
            )]))),
            ..State::default()
        };

        let view = initial_view(&settings, &state, Winner::State);
        assert!(view.columns.get(crate::dir_pane::Column::Permissions));
        assert!(
            !view.columns.get(crate::dir_pane::Column::Owner),
            "the remembered set wins whole, not merged with the configured one"
        );
        assert!(view.columns.get(crate::dir_pane::Column::Size));

        let view = initial_view(&settings, &state, Winner::Settings);
        assert!(view.columns.get(crate::dir_pane::Column::Owner));
        assert!(!view.columns.get(crate::dir_pane::Column::Size));
        assert!(!view.columns.get(crate::dir_pane::Column::Permissions));
    }

    #[test]
    fn state_overrides_settings_but_only_where_it_has_an_answer() {
        let settings: Settings = serde_json_lenient::from_str(
            r#"{"view": {"show_hidden": false, "folders_first": false}}"#,
        )
        .unwrap();
        let state = State {
            show_hidden: Some(true),
            ..State::default()
        };

        let view = initial_view(&settings, &state, Winner::State);
        assert!(view.show_hidden, "the toggle you last flipped wins");
        assert!(
            !view.folders_first,
            "what you never toggled stays configured"
        );

        // The other way round: the settings file is the newer of the two, so
        // what it says beats what was remembered, and where it says nothing,
        // the remembered value still stands.
        let view = initial_view(&settings, &state, Winner::Settings);
        assert!(
            !view.show_hidden,
            "an edit made while hoja was closed applies"
        );
        assert!(!view.folders_first);

        let state = State {
            sort: Some(SortSetting {
                key: SortKey::Size.into(),
                direction: SortDir::Descending.into(),
            }),
            ..State::default()
        };
        let view = initial_view(&settings, &state, Winner::Settings);
        assert!(
            matches!(view.sort.key, SortKey::Size),
            "the settings file has no sort to override it with"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hoja-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_save_publishes_only_what_this_instance_changed() {
        let path = temp_dir("overlay").join("state.json");

        // Another hoja got there first and set the sort.
        let theirs = State {
            sort: Some(SortSetting {
                key: SortKey::Size.into(),
                direction: SortDir::Descending.into(),
            }),
            ..State::default()
        };
        theirs.commit_at(
            &path,
            Dirty {
                sort: true,
                ..Dirty::default()
            },
        );

        // This one started before that, so its snapshot still says name-ascending,
        // but it only ever toggled hidden files.
        let ours = State {
            sort: Some(SortSetting::default()),
            show_hidden: Some(true),
            ..State::default()
        };
        ours.commit_at(
            &path,
            Dirty {
                show_hidden: true,
                ..Dirty::default()
            },
        );

        let merged = State::read(&path);
        assert_eq!(merged.show_hidden, Some(true), "our toggle landed");
        assert!(
            matches!(merged.sort.unwrap().key, SortKeyName::Size),
            "and did not revert theirs"
        );
    }

    #[test]
    fn nothing_dirty_writes_nothing() {
        let path = temp_dir("clean").join("state.json");
        State {
            show_hidden: Some(true),
            ..State::default()
        }
        .commit_at(&path, Dirty::default());
        assert!(
            !path.exists(),
            "an untouched instance leaves the file alone"
        );
    }

    #[test]
    fn concurrent_saves_do_not_lose_each_other() {
        // Each flock is taken on its own open file description, so threads
        // contend for it exactly as separate processes do.
        let path = temp_dir("race").join("state.json");
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut state = State::default();
                    state.column_widths.insert(format!("col{i}"), i as f32);
                    for _ in 0..20 {
                        state.commit_at(
                            &path,
                            Dirty {
                                column_widths: true,
                                ..Dirty::default()
                            },
                        );
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        // column_widths is one field, so the last writer legitimately owns it;
        // what must not happen is a torn or unparseable file.
        let merged = State::read(&path);
        assert_eq!(
            merged.column_widths.len(),
            1,
            "got {:?}",
            merged.column_widths
        );
    }

    #[test]
    fn interleaved_saves_of_different_fields_all_survive() {
        let path = temp_dir("fields").join("state.json");
        let sort = std::thread::spawn({
            let path = path.clone();
            move || {
                let state = State {
                    sort: Some(SortSetting {
                        key: SortKey::Kind.into(),
                        direction: SortDir::Ascending.into(),
                    }),
                    ..State::default()
                };
                for _ in 0..50 {
                    state.commit_at(
                        &path,
                        Dirty {
                            sort: true,
                            ..Dirty::default()
                        },
                    );
                }
            }
        });
        let hidden = std::thread::spawn({
            let path = path.clone();
            move || {
                let state = State {
                    show_hidden: Some(true),
                    ..State::default()
                };
                for _ in 0..50 {
                    state.commit_at(
                        &path,
                        Dirty {
                            show_hidden: true,
                            ..Dirty::default()
                        },
                    );
                }
            }
        });
        sort.join().unwrap();
        hidden.join().unwrap();

        let merged = State::read(&path);
        assert!(matches!(merged.sort.unwrap().key, SortKeyName::Kind));
        assert_eq!(merged.show_hidden, Some(true));
    }

    #[test]
    fn a_typo_is_reported_rather_than_ignored() {
        // deny_unknown_fields: a misspelled key is a mistake worth surfacing,
        // not a line that silently does nothing.
        let result: Result<Settings, _> =
            serde_json_lenient::from_str(r#"{"view": {"show_hiden": true}}"#);
        assert!(result.is_err());
    }

    #[test]
    fn sort_names_round_trip_through_their_on_disk_spelling() {
        let text = serde_json_lenient::to_string(&SortSetting {
            key: SortKey::Modified.into(),
            direction: SortDir::Descending.into(),
        })
        .unwrap();
        assert!(text.contains("\"modified\""), "got {text}");
        assert!(text.contains("\"descending\""), "got {text}");

        let back: SortSetting = serde_json_lenient::from_str(&text).unwrap();
        assert!(matches!(SortKey::from(back.key), SortKey::Modified));
        assert!(matches!(SortDir::from(back.direction), SortDir::Descending));
    }
}
