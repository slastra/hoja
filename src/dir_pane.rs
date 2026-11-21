use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use file_icons::FileIcons;
use gpui::AnimationExt as _;
use gpui::{
    App, Bounds, ClickEvent, Context, CursorStyle, DismissEvent, DragMoveEvent, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, SharedString, Subscription, Task, UniformListScrollHandle, Window, actions,
    anchored, deferred, div, prelude::*, px, uniform_list,
};
use hoja_transfer::Operation;
use theme::ActiveTheme;

use crate::file_menu::{FileMenu, MenuItem};
use crate::fs::{self, DirEntry, Sort, SortDir, SortKey, ViewSettings};
use crate::git::GitStatus;
use crate::history::{History, Place};
use crate::icon::Icon;

mod chrome;
mod columns;
mod drag;
mod grid;
mod list;
mod menus;

use self::columns::*;
pub use self::columns::{Column, ColumnLayout};
use self::drag::*;
use self::list::*;
use crate::location::Location;
use crate::path_editor::{PathEditor, PathEditorEvent};
use crate::workspace;

actions!(
    pane,
    [
        GoUp,
        OpenSelected,
        SelectAll,
        ClearSelection,
        MoveUp,
        MoveDown,
        MovePageUp,
        MovePageDown,
        MoveToTop,
        MoveToBottom,
        ExtendUp,
        ExtendDown,
        ExtendPageUp,
        ExtendPageDown,
        ExtendToTop,
        ExtendToBottom,
        CursorUp,
        CursorDown,
        MoveLeft,
        MoveRight,
        ExtendLeft,
        ExtendRight,
        CursorLeft,
        CursorRight,
        ToggleSelection,
        NavBack,
        NavForward,
        GoHome,
        EditPath,
        StartSearch,
        FocusResults,
        RenameSelected,
        ToggleHiddenFiles,
        ToggleFoldersFirst,
        ListView,
        GridView,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        Refresh,
        ReverseSort,
        SortByName,
        SortBySize,
        SortByKind,
        SortByModified,
        SortByPermissions,
        SortByOwner,
        SortByGroup,
    ]
);

/// Poll step for the directory watcher. Two of these bound how long a change
/// made elsewhere stays invisible, and also how often a busy directory re-lists.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

const ROW_HEIGHT: f32 = 22.;
const HEADER_HEIGHT: f32 = 24.;
/// Advance of one character at the rows' `text_sm`, and the `px_2` a cell
/// carries on each side. Only the width test reads them.
///
/// Two figures because the columns are no longer all in one face: the numeric
/// one is monospaced and every character is exactly this wide, while the
/// proportional one varies and this is an upper bound for the lowercase and
/// digits the columns actually hold.
#[cfg(test)]
const ROW_CHAR_W: f32 = 8.5;
#[cfg(test)]
const PROPORTIONAL_CHAR_W: f32 = 7.6;
#[cfg(test)]
const CELL_PADDING: f32 = 16.;

/// Emitted upward to the workspace. The workspace turns `Focus` into "this is now the
/// active pane": clicking a pane focuses it, and activation follows from that rather
/// than from separate click plumbing.
pub enum PaneEvent {
    Focus,
    /// Asks the workspace to remove this pane from the tree.
    #[allow(dead_code)]
    Remove,
    /// Something worth saying that has no job to attach to.
    ///
    /// `problem` because these are not all failures. "N names could not be
    /// shown" is an answer to a question, and colouring it like a broken
    /// transfer says the wrong thing about it.
    Notice {
        message: String,
        problem: bool,
    },
    /// A view setting or column width changed and is worth remembering.
    ViewChanged,
    /// A drop landed here. The workspace owns the engine and the job strip, so
    /// it starts the transfer.
    Transfer {
        op: Operation,
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// A file inside an archive was activated. Opening it means extracting a
    /// temporary copy first, which needs the user's say-so: the workspace owns
    /// the dialog that asks.
    ConfirmExtractAndOpen {
        archive: PathBuf,
        inside: PathBuf,
        /// The member's own path, relative to the archive's root.
        member: String,
        name: String,
    },
    /// Archive members were dropped onto a real directory. The workspace runs
    /// the same extract-then-move `extract_into` already does for a paste.
    ExtractDrop {
        archive: PathBuf,
        inside: PathBuf,
        roots: Vec<String>,
        dest: PathBuf,
    },
}

/// An open rename editor, anchored by name rather than by row.
///
/// A re-listing renumbers every row, so an index alone cannot survive one,
/// which is why a background copy finishing, or a delete in another pane, used
/// to close the editor out from under whoever was typing.
struct Renaming {
    /// The full path, not the name. In search results a name is not unique,
    /// three `README.md` under one root are three different files, so a
    /// re-listing that re-finds the row by name can move the open editor onto a
    /// file the user never touched, and enter then renames that one.
    path: PathBuf,
    ix: usize,
    editor: Entity<PathEditor>,
}

/// A search in flight, and the listing it is standing in for.
struct ActiveSearch {
    query: String,
    /// Put back when the search ends. Held rather than re-read, since a second
    /// copy of a large directory is not free but a second `read_dir` is worse.
    listing: Vec<DirEntry>,
    work: SearchWork,
}

/// A selection named by what it points at rather than by where it sits.
///
/// Indices name rows; a rebuild that re-sorts names different files with the
/// same numbers. All three have to travel together — the cursor says what
/// enter opens, the keys say what a copy takes, and the anchor says where a
/// shift-click measures from.
struct HeldSelection {
    keys: Vec<PathBuf>,
    cursor: Option<PathBuf>,
    anchor: Option<PathBuf>,
}

/// An archive a pane has read, and how much of it.
struct HeldIndex {
    /// Which archive, so a pane that has moved on does not answer for it.
    archive: PathBuf,
    index: Arc<crate::archive::Index>,
}

/// Which of the two kinds of search this is.
///
/// Stated rather than inferred. These used to be told apart by whether `poll`
/// happened to be `None`, which is a fact about how one of them is
/// implemented rather than about which one it is — and the next change to
/// either would have quietly made it wrong.
enum SearchWork {
    /// Searched through the arrangement the pane is holding. No walk, no
    /// thread, no debounce: finished before `set_filter` returned.
    Filtered {
        /// Whether `RESULT_CAP` cut the answer short, which the footer says.
        capped: bool,
    },
    /// A walk over real directories, arriving in batches.
    Walk {
        /// Dropping this stops the walk. `None` until the debounce elapses,
        /// which is the only window in which this search has no walker.
        handle: Option<crate::search::Search>,
        /// Dropping this stops the poll that drives `handle`, which is the
        /// whole of what it is for: nothing ever reads it, and holding it is
        /// how a new query abandons the previous one rather than racing it.
        #[allow(dead_code)]
        poll: Task<()>,
    },
}

/// What the address bar is being used for. Search reuses the same field
/// rather than adding a second one: there is one text slot in the toolbar and
/// two things worth typing into it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BarMode {
    Path,
    Search,
}

/// How long the selection or the directory must hold still before a walk is
/// worth starting.
///
/// The debounce sits between the keystroke and the *thread*, which is the whole
/// point: a held arrow key replaces this timer thirty times a second and starts
/// nothing at all, and the walk begins once, on the row you stopped on. The same
/// interval the search field settles for, for the same reason.
/// Half the finest bucket `fs::format_time_ago` prints, so nothing on screen is
/// ever more than that behind.
const CLOCK_TICK: Duration = Duration::from_secs(30);

const MEASURE_DEBOUNCE: Duration = Duration::from_millis(120);

/// How often an archive read is asked what it has found.
///
/// Fast enough that the first rows feel immediate, slow enough that a read
/// finishing in twenty milliseconds does not tick at all.
const ARCHIVE_POLL: Duration = Duration::from_millis(80);

/// How often the footer reads the running total. A size is a magnitude, not a
/// counter, it has to look alive, not be exact to the byte.
const MEASURE_POLL: Duration = Duration::from_millis(100);

/// The line at the bottom of the pane, and the walk behind its total.
///
/// One value rather than five fields, for the reason `ActiveSearch` is one:
/// every replacement has to drop the handle and the poll together, and a field
/// added later would otherwise have to be found in each teardown path.
#[derive(Default)]
struct Footer {
    /// What this was resolved against. A defaulted footer is out of date by
    /// construction: it has been resolved against nowhere.
    dir: Option<Location>,
    /// The listing the footer's summary was built for, and the directory read
    /// the walk belongs to. Two counters because they answer different
    /// questions: the summary has to follow a re-order, the walk must not.
    listing: u64,
    read: u64,
    /// The directories this walk covers, and where each sits among the walker's
    /// roots.
    ///
    /// Keyed by path for the same reason `settled` is, and it matters more here:
    /// a row index is a statement about the order the rows are in, and sorting
    /// is precisely the operation that invalidates every one of them while
    /// changing nothing about the walk underneath.
    roots: HashMap<PathBuf, usize>,
    /// Folder sizes that are final, keyed by path rather than by row.
    ///
    /// By path because the one thing a settled walk earns is a re-sort, and
    /// sorting moves every index the buckets were found under. A path survives
    /// it; a row number does not.
    settled: HashMap<PathBuf, u64>,
    /// Whether the one re-sort a settled walk earns has happened yet.
    resorted: bool,
    selection: u64,
    /// The columns the summary was built against. The line for a single row
    /// says the things no column shows, so showing or hiding one changes what
    /// it has to say without touching the listing or the selection, and
    /// neither counter above would notice.
    columns: ColumnLayout,
    summary: fs::Summary,
    /// The line as it reads now. Held rather than rebuilt per frame, and
    /// compared against by the poll so a total that has not moved a printed
    /// digit does not repaint the pane.
    text: String,
    /// `None` until the debounce elapses, and for good when the listing already
    /// held the whole answer. Dropping it stops the walk.
    walk: Option<crate::measure::Measure>,
    /// Dropping this stops the poll, and with it any walk the debounce had not
    /// yet let it start.
    poll: Option<Task<()>>,
}

/// How much of the selection colour survives in a pane that is not active.
const INACTIVE_SELECTION_ALPHA: f32 = 0.5;

/// How much of the listing's colour survives there.
///
/// Higher than the selection's: a selection band only has to be noticed, but a
/// file name still has to be read.
const INACTIVE_CONTENT_ALPHA: f32 = 0.6;

/// The same colour, quieter, for a pane the keys are not acting on.
///
/// Alpha rather than substituting `text_muted`, so a name coloured by its git
/// status keeps its hue: dimming the listing should not cost the listing its
/// information.
/// The card width a grid aims at until it is zoomed.
pub const DEFAULT_CARD_WIDTH: u16 = 220;
/// The widths zooming steps through, smallest first. The default is one.
const CARD_WIDTHS: [u16; 5] = [140, 180, DEFAULT_CARD_WIDTH, 280, 360];

/// A card width from a settings file, held to what the zoom steps span.
pub fn clamp_card_width(width: u16) -> u16 {
    width.clamp(CARD_WIDTHS[0], CARD_WIDTHS[CARD_WIDTHS.len() - 1])
}

/// The next zoom step from `width`: larger when `bigger`, smaller if not.
/// From a width between steps, the next step that way; from the last step,
/// the same.
fn zoom_step(width: u16, bigger: bool) -> u16 {
    if bigger {
        CARD_WIDTHS
            .into_iter()
            .find(|&w| w > width)
            .unwrap_or(width)
    } else {
        CARD_WIDTHS
            .into_iter()
            .rev()
            .find(|&w| w < width)
            .unwrap_or(width)
    }
}

fn quieted(mut color: gpui::Hsla, active: bool) -> gpui::Hsla {
    if !active {
        color.a *= INACTIVE_CONTENT_ALPHA;
    }
    color
}

/// How `DirPane::scroll_to` brings an entry into view.
#[derive(Clone, Copy)]
enum Reveal {
    /// The least scroll that shows it.
    Nearest,
    /// At the top of the view, as far as the end of the listing allows.
    Top,
    /// At the top of the view even when that scrolls past the end, which is
    /// how Back puts a scroll position back exactly.
    TopEvenPastTheEnd,
}

/// A keyboard movement of the lead row. Each has a plain form that takes the
/// selection with it and a shift form that extends the selection instead.
#[derive(Clone, Copy)]
enum Motion {
    Up,
    Down,
    /// Sideways, which only means something where entries sit side by side.
    Left,
    Right,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

pub struct DirPane {
    focus_handle: FocusHandle,
    /// Whether this is the pane the keys act on.
    ///
    /// Told by the workspace rather than read from focus: a pane stays the
    /// active one while the command palette holds the keyboard, and asking
    /// `contains_focused` would dim every pane the moment a modal opened.
    active: bool,
    dir: Location,
    history: History,
    entries: Vec<DirEntry>,
    /// Multi-select: plain click replaces, ctrl-click toggles, shift-click
    /// extends a range from the anchor, ctrl-a selects all.
    selected: fs::Selection,
    /// The row a shift-range extends from. Stays put while the lead moves.
    anchor_ix: Option<usize>,
    /// The lead row: what Rename, Open, and the context menu act on, and what
    /// the arrow keys move. Equal to `anchor_ix` except while a range is being
    /// extended: shift-down then shift-up has to *shrink* the range, which is
    /// only possible with the two ends tracked separately.
    cursor_ix: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Per-pane, because panes in a split can be very different widths.
    widths: ColumnWidths,
    /// Anchor for the resize in progress, if any.
    resize: Option<ColumnDrag>,
    /// The same for a header being dragged to a new slot.
    ///
    /// Cleared on release, unlike `resize`, and by two handlers rather than
    /// one so that a drag ending outside the header still counts. It has to
    /// be: the tint saying which column is being carried is drawn from this,
    /// so state that outlived the gesture would be paint that did too.
    moving: Option<ColumnMoveDrag>,
    /// Whether the scrollbar is mid-drag. The bar is rebuilt every frame; a
    /// drag is not.
    scrollbar: crate::scrollbar::ScrollbarState,
    /// The grid layout's scroll and its cached layout. See `grid`.
    grid_scroll: gpui::ScrollHandle,
    grid: std::cell::RefCell<grid::GridState>,
    /// Git status by entry name for the loaded directory. Empty outside a
    /// repository, and empty until the background query lands.
    git: crate::git::GitStatuses,
    /// The directory `git` is the statuses of.
    git_for: Option<PathBuf>,
    git_task: Option<Task<()>>,
    /// The repository's git directory, watched. See `watch_git`.
    git_watch: Option<(PathBuf, Task<()>)>,
    /// The directory the change watcher is armed on, paired with its task, so
    /// re-listing the same directory does not tear down and rebuild the inotify
    /// watch, and so a watcher that failed to arm cannot be recorded as armed.
    watch: Option<(PathBuf, Task<()>)>,
    view: ViewSettings,
    /// The location `entries` was built from. Differs from `dir` while a load
    /// is in flight, which is how a navigation is told apart from a refresh.
    ///
    /// `None` before the first listing has landed.
    loaded_dir: Option<Location>,
    /// Set when the directory could not be read at all (permissions, gone, not a dir).
    error: Option<String>,
    /// Held so that navigating away cancels an in-flight read by dropping its task.
    load_task: Option<Task<()>>,
    /// The arranged archive this pane is showing, kept from the read that
    /// listed it.
    ///
    /// Cleared on leaving. Shared with `archive`'s own cache rather than a
    /// second copy of it, so once a read finishes this costs a pointer: what
    /// it buys is that the listing stays available at all. The cache holds two
    /// archives and this pane's watcher drops entries whenever anything beside
    /// the file changes — a download finishing in the same directory is
    /// enough — so nothing that wants the member list later can rely on it
    /// still being there.
    held: Option<HeldIndex>,
    /// Uncompressed bytes an archive read has accounted for, while it runs.
    ///
    /// `None` when nothing is being read, which is what the footer reads as
    /// "this is the whole listing" rather than "so far".
    reading_bytes: Option<u64>,
    /// Stops the read behind `load_task`, which dropping the task does not.
    ///
    /// Only an archive read is slow enough to matter: `read_dir` returns in
    /// milliseconds, where listing a tarball has to decompress the whole thing.
    reading: Option<crate::archive::Cancel>,
    /// Separate from `load_task` so a header click cannot cancel a pending read.
    sort_task: Option<Task<()>>,
    /// Repaints the listing so the Modified column stays true. Dropped with the
    /// pane, which stops it.
    _clock: Task<()>,
    context_menu: Option<(Point<Pixels>, Entity<FileMenu>)>,
    /// The address bar doubles as the search field, so both live in one slot.
    path_editor: Option<(BarMode, Entity<PathEditor>)>,
    /// One value, because these are one fact. Four parallel `Option`s had to be
    /// cleared together by hand in every teardown path, and a fifth field would
    /// have meant finding them all again.
    search: Option<ActiveSearch>,
    /// Inline rename: the row index and its editor.
    renaming: Option<Renaming>,
    /// Names to re-select once the listing is rebuilt. Every path that
    /// replaces `entries` snapshots the current selection into this, so
    /// sorting, refreshing, or a job finishing elsewhere does not silently
    /// drop what the user had selected.
    /// Paths, not names: a search listing holds entries from many directories,
    /// where a name identifies nothing.
    pending_select: Vec<PathBuf>,
    /// Where to stand in the next listing, when Back or Forward returns to
    /// one that was left. Consumed by `restore_selection`.
    pending_place: Option<Place>,
    /// Bumped whenever the rows themselves are replaced. Separate from
    /// `selection_moved` because the walk belongs to the listing: moving the
    /// selection must not restart it, and a re-listing must.
    listing_moved: u64,
    /// Bumped only when the directory has actually been read from disk again,
    /// which `listing_moved` cannot say: sorting bumps that and touches no file.
    /// The folder-size walk keys off this, because a re-order tells it nothing
    /// while a re-read means every size it holds may now be wrong.
    directory_read: u64,
    footer: Footer,
    /// Type-ahead find: the last keystroke's time and the accumulated prefix.
    /// One value, so "no buffer" cannot disagree with "no timestamp".
    type_ahead: Option<(Instant, String)>,
    _subscriptions: Vec<Subscription>,
}

impl DirPane {
    pub fn new(
        dir: Location,
        view: ViewSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // New panes are made active by the workspace as it focuses them; the
        // very first one starts active because it is.
        let subscriptions = vec![
            cx.on_focus_in(&focus_handle, window, |_, _, cx| {
                cx.emit(PaneEvent::Focus);
            }),
            // The previews this pane's grid wanted are not pinned once it is
            // gone.
            cx.on_release({
                let id = cx.entity_id();
                move |_, cx| crate::preview::forget(id, cx)
            }),
        ];

        // Focus on creation so key bindings resolve without a click first.
        window.focus(&focus_handle, cx);

        let history = History::new(dir.clone());
        let mut this = Self {
            focus_handle,
            active: true,
            dir,
            history,
            entries: Vec::new(),
            selected: fs::Selection::default(),
            anchor_ix: None,
            cursor_ix: None,
            scroll: UniformListScrollHandle::new(),
            widths: ColumnWidths::default(),
            resize: None,
            moving: None,
            scrollbar: crate::scrollbar::ScrollbarState::default(),
            grid_scroll: gpui::ScrollHandle::new(),
            grid: Default::default(),
            git: crate::git::GitStatuses::new(),
            git_for: None,
            git_task: None,
            git_watch: None,
            watch: None,
            view,
            loaded_dir: None,
            error: None,
            load_task: None,
            held: None,
            reading_bytes: None,
            reading: None,
            sort_task: None,
            _clock: Self::spawn_clock(cx),
            context_menu: None,
            path_editor: None,
            search: None,
            renaming: None,
            pending_select: Vec::new(),
            pending_place: None,
            listing_moved: 0,
            directory_read: 0,
            footer: Footer::default(),
            type_ahead: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// Keep the Modified column honest.
    ///
    /// "3 hours ago" is a statement about when it was rendered, and gpui repaints
    /// on interaction, so a pane left alone would hold whatever it said when you
    /// last touched it. A file written a moment before you walked away would
    /// still read "just now" an hour later.
    ///
    /// The interval is half the finest bucket, so the worst reading on screen is
    /// half a minute behind. It costs one repaint every thirty seconds for as
    /// long as the pane exists, which is the price of a column that says "ago".
    fn spawn_clock(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CLOCK_TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
    }

    /// Mark this pane active or not. The workspace owns which one it is.
    /// What this pane is showing, for `HOJA_PROBE`. See `crate::probe`.
    ///
    /// The columns are read through `Column::value`, the same call the rows
    /// render through, so a test asserts the text a person reads rather than
    /// the bytes behind it. A probe built from the underlying data instead
    /// would pass while the screen showed something else, which is the one
    /// thing it must not do.
    pub fn probe(&self, now: std::time::SystemTime, cx: &App) -> crate::probe::PaneProbe {
        let column = |which: Column| {
            self.entries
                .iter()
                .take(crate::probe::MAX_ROWS)
                .enumerate()
                .map(|(ix, entry)| which.value(entry, self.folder_bytes(ix), now))
                .collect()
        };
        let menu = self.context_menu.as_ref().map(|(_, menu)| menu.read(cx));
        let submenu = menu.and_then(|menu| menu.open_submenu_labels());
        crate::probe::PaneProbe {
            dir: self.dir.clone(),
            active: self.active,
            row_count: self.entries.len(),
            rows: self
                .entries
                .iter()
                .take(crate::probe::MAX_ROWS)
                .map(|entry| entry.relative.clone().unwrap_or_else(|| entry.name.clone()))
                .collect(),
            sizes: column(Column::Size),
            modified: column(Column::Modified),
            permissions: column(Column::Permissions),
            git: self
                .entries
                .iter()
                .take(crate::probe::MAX_ROWS)
                .map(|entry| {
                    let status = self.git.get(&entry.name).copied();
                    status.map_or("", GitStatus::name).to_string()
                })
                .collect(),
            columns: self
                .view
                .columns
                .iter()
                .map(|column| column.label().to_string())
                .collect(),
            selected: self.selected.iter().collect(),
            cursor: self.cursor_ix,
            footer: self.footer_line(),
            counting: (0..self.entries.len().min(crate::probe::MAX_ROWS))
                .filter(|ix| self.counting_size(*ix))
                .collect(),
            searching: self.searching(),
            search_field: matches!(self.path_editor, Some((BarMode::Search, _))),
            top_row: self.top_row(),
            layout: match self.view.layout {
                fs::Layout::List => "list",
                fs::Layout::Grid => "grid",
            },
            grid_columns: match self.view.layout {
                fs::Layout::List => 0,
                fs::Layout::Grid => self.grid_columns_measured(),
            },
            card_details: self.view.card_details.names(),
            cards: match self.view.layout {
                fs::Layout::List => Vec::new(),
                fs::Layout::Grid => self.grid_cards(cx),
            },
            reading: self.reading_bytes.is_some(),
            error: self.error.clone(),
            menu_open: self.context_menu.is_some(),
            menu_items: menu.map(|menu| menu.item_labels()).unwrap_or_default(),
            menu_selected: menu.and_then(|menu| menu.selected_label()),
            submenu_open: submenu.is_some(),
            submenu_items: submenu.unwrap_or_default(),
        }
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active != active {
            self.active = active;
            cx.notify();
        }
    }

    /// Where this pane is, which is not always a directory.
    pub fn location(&self) -> &Location {
        &self.dir
    }

    /// The real directory this pane is showing, and `None` inside an archive.
    pub fn disk_dir(&self) -> Option<&Path> {
        self.dir.disk()
    }

    /// Splits copy the source pane's view settings, sort included.
    pub fn view_settings(&self) -> ViewSettings {
        self.view
    }

    /// Replace the view wholesale, as a settings edit does.
    ///
    /// Re-lists only where the rows themselves are in question: hidden files
    /// and the sort order both change what they are and what order they come
    /// in, and the columns change neither, which is the same thing
    /// `toggle_column` says from the other side. Without the distinction,
    /// saving `settings.json` re-read every directory in every pane to show or
    /// hide one column.
    pub fn set_view_settings(&mut self, view: ViewSettings, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        let rows_change = self.view.show_hidden != view.show_hidden
            || self.view.folders_first != view.folders_first
            || self.view.sort != view.sort;
        self.view = view;
        if rows_change {
            self.relist(cx);
        } else {
            cx.notify();
        }
    }

    pub fn column_widths(&self) -> std::collections::HashMap<String, f32> {
        Column::ALL
            .into_iter()
            .map(|column| (column.key().to_string(), f32::from(self.widths.get(column))))
            .collect()
    }

    /// Apply widths remembered from a previous run. Unknown keys are ignored,
    /// so a column that no longer exists cannot resurrect itself.
    pub fn set_column_widths(&mut self, widths: &std::collections::HashMap<String, f32>) {
        for column in Column::ALL {
            if let Some(width) = widths.get(column.key()) {
                self.widths.set(column, px(*width));
            }
        }
    }

    fn toggle_hidden(&mut self, _: &ToggleHiddenFiles, _w: &mut Window, cx: &mut Context<Self>) {
        self.view.show_hidden = !self.view.show_hidden;
        cx.emit(PaneEvent::ViewChanged);
        self.relist(cx);
    }

    /// Forget the header drag, so the tint stops with the gesture.
    ///
    /// The order it produced stands: it was applied to `view.columns` as the
    /// pointer moved and there is nothing to commit here. `remember_view` has
    /// already been told, on every one of those moves.
    fn finish_move(&mut self, cx: &mut Context<Self>) {
        if self.moving.take().is_some() {
            cx.notify();
        }
    }

    /// The column being carried to a new slot, if one is.
    ///
    /// `moving` is set by any mouse down on a header, a plain click included,
    /// so `dragged` is what separates a drag from a press. Cleared on release
    /// by the header's two mouse-up handlers, which is what stops the tint
    /// outliving the gesture.
    fn moving_column(&self) -> Option<Column> {
        self.moving
            .as_ref()
            .filter(|drag| drag.dragged)
            .map(|drag| drag.column)
    }

    /// Show or hide one column.
    ///
    /// No relist and no re-sort: which rows exist and what order they are in
    /// have not changed, only which facts about them are drawn. A column that
    /// the listing is currently sorted by can be hidden, and the order stays
    /// as it was, which is what was asked for even once the chevron saying so
    /// has gone.
    /// Turn a detail on or off in the set the layout in force draws: a column
    /// of the list, or a line on each card of the grid.
    fn toggle_column(&mut self, column: Column, cx: &mut Context<Self>) {
        match self.view.layout {
            fs::Layout::List => self.view.columns.toggle(column),
            fs::Layout::Grid => self.view.card_details.toggle(column),
        }
        // The menu this came from is still up, so its check marks are now out
        // of date. Only the view menu can reach here, the Columns list being
        // the only place a column is toggled, so the rows it is handed back are
        // the right ones.
        //
        // Deferred, and that is not a nicety: this runs from inside the menu's
        // own click handler, so the menu is already being updated and gpui
        // panics outright rather than reentering it. Nothing here reads the
        // menu, so there is nothing to be stale by waiting.
        if let Some((_, menu)) = self.context_menu.clone() {
            let pane = cx.entity().downgrade();
            cx.defer(move |cx| {
                let Some(pane) = pane.upgrade() else {
                    return;
                };
                let items = pane.update(cx, |pane, cx| pane.settings_menu_items(cx));
                menu.update(cx, |menu, cx| menu.refresh_items(items, cx));
            });
        }
        // The footer follows from `sync_footer`, which compares rather than
        // being told: the same change also arrives from the settings file,
        // and a line rebuilt here alone would be right for one of the two
        // ways in and stale for the other.
        cx.emit(PaneEvent::ViewChanged);
        cx.notify();
    }

    /// Show this pane as a list or as a grid.
    ///
    /// The entry under the cursor, or failing that the one at the top, stays
    /// in view across the change, so switching does not lose your place.
    fn set_layout(&mut self, layout: fs::Layout, cx: &mut Context<Self>) {
        if self.view.layout == layout {
            return;
        }
        let keep = match self.cursor_ix {
            Some(ix) => Some((ix, Reveal::Nearest)),
            None => Some((self.top_row(), Reveal::Top)),
        };
        self.view.layout = layout;
        if let Some((ix, how)) = keep.filter(|(ix, _)| *ix < self.entries.len()) {
            self.scroll_to(ix, how);
        }
        cx.emit(PaneEvent::ViewChanged);
        cx.notify();
    }

    /// Make the grid's cards larger or smaller, or back to the default. The
    /// list has nothing to zoom, so there it does nothing.
    fn zoom(&mut self, to: Option<bool>, cx: &mut Context<Self>) {
        if self.view.layout != fs::Layout::Grid {
            return;
        }
        let width = match to {
            Some(bigger) => zoom_step(self.view.card_width, bigger),
            None => DEFAULT_CARD_WIDTH,
        };
        if width == self.view.card_width {
            return;
        }
        self.view.card_width = width;
        // The card under the cursor stays in view at the new size. Without a
        // cursor, the first card in view keeps its place: see `grid`.
        if let Some(ix) = self.cursor_ix.filter(|&ix| ix < self.entries.len()) {
            self.scroll_to(ix, Reveal::Nearest);
        }
        cx.emit(PaneEvent::ViewChanged);
        cx.notify();
    }

    fn toggle_folders_first(
        &mut self,
        _: &ToggleFoldersFirst,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view.folders_first = !self.view.folders_first;
        cx.emit(PaneEvent::ViewChanged);
        self.apply_sort(cx);
    }

    fn refresh_action(&mut self, _: &Refresh, _w: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn sort_by_name(&mut self, _: &SortByName, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Name, cx);
    }
    fn sort_by_size(&mut self, _: &SortBySize, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Size, cx);
    }
    fn sort_by_kind(&mut self, _: &SortByKind, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Kind, cx);
    }
    fn sort_by_modified(&mut self, _: &SortByModified, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Modified, cx);
    }
    fn sort_by_permissions(
        &mut self,
        _: &SortByPermissions,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_sort_key(SortKey::Permissions, cx);
    }
    fn sort_by_owner(&mut self, _: &SortByOwner, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Owner, cx);
    }
    fn sort_by_group(&mut self, _: &SortByGroup, _w: &mut Window, cx: &mut Context<Self>) {
        self.select_sort_key(SortKey::Group, cx);
    }

    /// Identity keys for the current selection, in listing order.
    ///
    /// For remembering a selection across a re-listing, and for nothing that
    /// touches a file: see `DirEntry::key`.
    pub fn selected_keys(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter_map(|ix| self.entries.get(ix))
            .map(|e| e.key().to_path_buf())
            .collect()
    }

    /// The archive this pane is in, the directory within it, and the members
    /// the selection names. `None` unless the pane is inside an archive.
    ///
    /// The member paths are relative to the archive's root and name folders as
    /// well as files: a folder in an archive is a prefix rather than a thing,
    /// because three of twelve real zip files name no folders at all, so
    /// expanding one is `archive::extract`'s job and not this one's.
    pub fn selected_in_archive(&self) -> Option<(PathBuf, PathBuf, Vec<String>)> {
        let Location::Archive { archive, inside } = &self.dir else {
            return None;
        };
        let roots = self
            .selected
            .iter()
            .filter_map(|ix| self.entries.get(ix))
            .filter_map(|entry| entry.key().strip_prefix(archive).ok())
            .map(|member| member.to_string_lossy().into_owned())
            .collect();
        Some((archive.clone(), inside.clone(), roots))
    }

    /// Real paths for the current selection, and `None` when any row of it has
    /// none.
    ///
    /// All or nothing on purpose. A selection that spans rows with paths and
    /// rows without them can only be half copied or half deleted, and half of
    /// either is worse than refusing the lot.
    pub fn selected_on_disk(&self) -> Option<Vec<PathBuf>> {
        self.selected
            .iter()
            .filter_map(|ix| self.entries.get(ix))
            .map(|e| e.on_disk().map(Path::to_path_buf))
            .collect()
    }

    /// Re-read the directory (used by the workspace when a job completes here).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// Select these paths once the next listing lands.
    pub fn select_on_next_load(&mut self, paths: Vec<PathBuf>) {
        self.pending_select = paths;
    }

    /// Aim the selection at whatever survives `removed`, then re-list.
    ///
    /// Deleting the row under the cursor and landing on nothing loses your
    /// place in a long listing, so the selection walks forward to the next
    /// survivor, or back to the previous one when the removed items were at
    /// the end. The listing still holds the departing entries at this point,
    /// which is what makes "next" meaningful.
    pub fn select_after_removal(&mut self, removed: &[PathBuf], cx: &mut Context<Self>) {
        let gone: std::collections::HashSet<&Path> =
            removed.iter().map(|path| path.as_path()).collect();
        let surviving = |entry: &DirEntry| !gone.contains(entry.key());

        let first_gone = self
            .entries
            .iter()
            .position(|entry| !surviving(entry))
            .unwrap_or(0);
        let successor = self.entries[first_gone..]
            .iter()
            .find(|entry| surviving(entry))
            .or_else(|| {
                self.entries[..first_gone]
                    .iter()
                    .rev()
                    .find(|e| surviving(e))
            });

        self.pending_select = successor
            .map(|entry| vec![entry.key().to_path_buf()])
            .unwrap_or_default();
        self.reload(cx);
    }

    /// True while an inline editor owns the keyboard. New editors must be added
    /// here rather than to each dismiss handler.
    fn has_inline_editor(&self) -> bool {
        self.renaming.is_some() || self.path_editor.is_some()
    }

    /// Type-ahead: handle a printable key. Returns true when consumed.
    fn type_ahead_key(&mut self, key_char: &str, cx: &mut Context<Self>) -> bool {
        const TIMEOUT: Duration = Duration::from_millis(1000);
        let now = Instant::now();
        // An expired buffer is already gone as far as the caller is concerned.
        let mut buffer = match self.type_ahead.take() {
            Some((last, buf)) if now.duration_since(last) <= TIMEOUT => buf,
            _ => String::new(),
        };
        // Reserve bare space for future use (preview); mid-buffer spaces are
        // legitimate name characters ("New Folder").
        if key_char == " " && buffer.is_empty() {
            return false;
        }
        buffer.push_str(key_char);

        if let Some(ix) = fs::type_ahead_target(&self.entries, &buffer, self.cursor_ix) {
            self.selected.only(ix);
            self.place_cursor(ix);
            self.scroll_to(ix, Reveal::Nearest);
            cx.notify();
        }
        self.type_ahead = Some((now, buffer));
        true
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected.set((0..self.entries.len()).collect());
        if self.cursor_ix.is_none() && !self.entries.is_empty() {
            self.place_cursor(0);
        }
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, window: &mut Window, cx: &mut Context<Self>) {
        // A menu is open but the key came here, so the menu does not have focus
        // yet: it takes it two frames after it opens (see `show_menu`). An
        // Escape pressed in that gap was meant for the menu.
        if self.context_menu.take().is_some() {
            cx.notify();
            return;
        }
        // Escape backs out one level at a time: the search before the
        // selection, so results are not dismissed together with what you picked
        // out of them.
        if self.searching() {
            self.end_search(window, cx);
            return;
        }
        self.selected.clear();
        self.anchor_ix = None;
        self.cursor_ix = None;
        self.type_ahead = None;
        cx.notify();
    }

    /// Put both ends of the selection on `ix`. Every path except a shift-range
    /// wants this: the row becomes the lead *and* the origin a later range
    /// extends from.
    fn place_cursor(&mut self, ix: usize) {
        self.anchor_ix = Some(ix);
        self.cursor_ix = Some(ix);
    }

    /// Rows a page key moves, from the last layout.
    ///
    /// One row short of a screenful, so a page keeps a line of context and you
    /// can tell where you came from, the convention in every list and editor.
    /// The size is unknown only before the first layout, which cannot happen
    /// before there is a window to press a key in.
    fn rows_per_page(&self) -> usize {
        // `ItemSize.item` is NOT a row: uniform_list stores the list's padded
        // viewport there, and puts row_height * item_count in `contents`. The
        // field name invites dividing the viewport by itself, which yields a
        // one-row page.
        let len = self.entries.len();
        let state = self.scroll.0.borrow();
        let Some(size) = state.last_item_size else {
            return 1;
        };
        if len == 0 {
            return 1;
        }
        let row = size.contents.height / len as f32;
        if row <= px(0.) {
            return 1;
        }
        ((size.item.height / row) as usize).saturating_sub(1).max(1)
    }

    /// Entries side by side in one band of the layout: up and down step by
    /// this many, and left and right move only when it is more than one. In
    /// the list it is one.
    fn across(&self) -> usize {
        match self.view.layout {
            fs::Layout::List => 1,
            fs::Layout::Grid => self.grid_columns(),
        }
    }

    /// Where `motion` lands, or `None` when the listing is empty or the
    /// layout has nowhere for it to go.
    fn destination(&self, motion: Motion) -> Option<usize> {
        let len = self.entries.len();
        let step = |delta: isize| fs::step_row(len, self.cursor_ix, delta);
        let across = self.across();
        match motion {
            Motion::Up => step(-(across as isize)),
            Motion::Down => step(across as isize),
            Motion::Left if across > 1 => step(-1),
            Motion::Right if across > 1 => step(1),
            Motion::Left | Motion::Right => None,
            // A page in the grid stays in its column, so it is a card a view
            // away rather than a count of entries.
            Motion::PageUp | Motion::PageDown if self.view.layout == fs::Layout::Grid => {
                let down = matches!(motion, Motion::PageDown);
                match self.cursor_ix.filter(|&ix| ix < len) {
                    Some(from) => self.grid_page(from, down).or(Some(from)),
                    // No lead yet: as the list does.
                    None => step(if down { 1 } else { -1 }),
                }
            }
            Motion::PageUp => step(-(self.rows_per_page() as isize)),
            Motion::PageDown => step(self.rows_per_page() as isize),
            Motion::Top => (len > 0).then_some(0),
            Motion::Bottom => len.checked_sub(1),
        }
    }

    /// Move the lead and take the selection with it.
    fn move_cursor(&mut self, motion: Motion, cx: &mut Context<Self>) {
        let Some(ix) = self.destination(motion) else {
            return;
        };
        self.selected.only(ix);
        self.place_cursor(ix);
        self.reveal(ix, cx);
    }

    /// Move the lead and leave the selection alone, so a row can be added to a
    /// scattered selection without disturbing what is already in it. Pointless
    /// without the lead ring drawn in `render_row`: otherwise the next
    /// `ToggleSelection` is a blind guess.
    fn focus_cursor(&mut self, motion: Motion, cx: &mut Context<Self>) {
        let Some(ix) = self.destination(motion) else {
            return;
        };
        self.place_cursor(ix);
        self.reveal(ix, cx);
    }

    /// Add or remove the lead row, the keyboard twin of a control-click.
    fn toggle_selection(&mut self, _: &ToggleSelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.cursor_ix.filter(|&ix| ix < self.entries.len()) else {
            return;
        };
        self.selected.toggle(ix);
        self.place_cursor(ix);
        cx.notify();
    }

    /// Move the lead, select everything back to the anchor.
    fn extend_cursor(&mut self, motion: Motion, cx: &mut Context<Self>) {
        let Some(ix) = self.destination(motion) else {
            return;
        };
        // A shift-arrow with nothing selected yet anchors where it starts, so
        // the range grows from there rather than from row zero.
        let anchor = self
            .anchor_ix
            .filter(|&a| a < self.entries.len())
            .unwrap_or(ix);
        self.anchor_ix = Some(anchor);
        self.cursor_ix = Some(ix);
        self.selected
            .set((anchor.min(ix)..=anchor.max(ix)).collect());
        self.reveal(ix, cx);
    }

    fn reveal(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroll_to(ix, Reveal::Nearest);
        cx.notify();
    }

    /// Scroll so that entry `ix` is in view, as `how` says.
    ///
    /// Every scroll goes through here, and the three measures below it
    /// (`top_row`, `visible_rows`, `rows_per_page`) are the only other places
    /// that know how entries are laid out. A layout other than the list
    /// answers these and nothing else.
    fn scroll_to(&self, ix: usize, how: Reveal) {
        if self.view.layout == fs::Layout::Grid {
            return self.grid_scroll_to(ix, how);
        }
        match how {
            Reveal::Nearest => self
                .scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest),
            Reveal::Top => self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Top),
            Reveal::TopEvenPastTheEnd => self
                .scroll
                .scroll_to_item_strict(ix, gpui::ScrollStrategy::Top),
        }
    }

    /// Read the current directory on a background thread, then apply on the UI thread.
    ///
    /// Assigning to `self.load_task` drops any previous task, which cancels a read that
    /// is still running, so hammering navigation cannot land stale entries.
    /// Re-list without disturbing anything a re-listing cannot change.
    ///
    /// A view-setting change (hidden files, folder grouping) rebuilds the
    /// rows but cannot alter a file's git status, and re-asking costs two
    /// process spawns and a work-tree walk. It also blanks `self.git` first, so
    /// every name would flash back to its default colour and back again.
    fn relist(&mut self, cx: &mut Context<Self>) {
        self.reload_inner(false, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_inner(true, cx);
    }

    /// A reload the *watcher* asked for, which must not disturb what the user
    /// is in the middle of.
    ///
    /// `reload_inner` drops any running search, because a fresh listing has to
    /// replace the results on screen: fine when you asked for it, wrong when a
    /// background write in the directory did. Downloading a file into the
    /// folder you are searching should not close the search.
    ///
    /// The listing behind the search does go stale while it is skipped. That is
    /// the smaller wrong: ending the search re-reads it (see `set_filter`).
    fn reload_from_watch(&mut self, cx: &mut Context<Self>) {
        if self.searching() || self.has_inline_editor() {
            return;
        }
        self.reload(cx);
    }

    fn reload_inner(&mut self, refresh_git: bool, cx: &mut Context<Self>) {
        // A directory change starts fresh; a refresh of the same directory
        // keeps the selection.
        if self.pending_select.is_empty() && self.loaded_dir.as_ref() == Some(&self.dir) {
            self.pending_select = self.selected_keys();
        }
        if refresh_git {
            self.reload_git(cx);
        }
        self.watch_dir(cx);
        // A listing read would otherwise land on top of search results. The
        // saved listing is discarded rather than restored: this read replaces
        // it anyway.
        if self.search.take().is_some()
            && let Some((BarMode::Search, _)) = &self.path_editor
        {
            self.path_editor = None;
        }

        let dir = self.dir.clone();
        let sort = self.view.sort;
        let show_hidden = self.view.show_hidden;
        let folders_first = self.view.folders_first;

        // Dropping the task discards the answer; it does not stop a read
        // already running. That is fine for a directory and not for an archive,
        // where the read can be a full decompression, so navigating away says
        // so rather than leaving a thread inflating something nobody wants.
        let cancel = crate::archive::Cancel::new();
        if let Some(previous) = self.reading.replace(cancel.clone()) {
            previous.stop();
        }
        // Reset here, not left for the old read's own "finished" branch to
        // clear: navigating away drops that task before it ever reaches it,
        // abandoned mid-loop rather than completed, so nothing else would ever
        // set this back to `None`. Left stuck at a stale `Some`, it would keep
        // this and every later directory's footer reading "reading… N items"
        // forever, for a load that has nothing to do with any archive.
        self.reading_bytes = None;
        // Whatever is arranged now belongs to where the pane was, not where it
        // is going. Dropped here rather than on arrival so leaving an archive
        // releases it immediately, which is when its megabytes stop earning
        // their keep.
        self.held = None;

        // An archive is read a piece at a time, because reading one can take a
        // minute and a pane showing nothing for a minute looks broken. See
        // `spawn_archive_read`.
        if let Location::Archive { archive, inside } = &self.dir {
            let (archive, inside) = (archive.clone(), inside.clone());
            self.load_task = Some(self.spawn_archive_read(archive, inside, cancel, cx));
            return;
        }

        self.load_task = Some(cx.spawn(async move |this, cx| {
            // `read_dir` is blocking and sorting a large listing costs ~100ms, so both
            // run off the foreground executor in the same hop.
            let result = cx
                .background_spawn(async move {
                    // `skipped` counts the names an archive holds that cannot
                    // be shown: refused by `tidy`, or a repeat of one already
                    // taken. Zero for a directory, which has neither.
                    let (mut entries, skipped) = match dir.disk() {
                        Some(dir) => (fs::read_dir(dir, show_hidden)?, 0),
                        // Archives took the branch above.
                        None => (Vec::new(), 0),
                    };
                    fs::sort_entries(&mut entries, sort, folders_first);
                    anyhow::Ok((entries, skipped))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((entries, skipped)) => {
                        this.entries = entries;
                        this.listing_moved += 1;
                        this.directory_read += 1;
                        this.error = None;
                        // Said once, on the listing that found them. The names
                        // themselves are exactly the ones there is no safe way
                        // to show, so the count is the whole of what can be
                        // reported: the alternative is a view that quietly
                        // claims to be the whole archive.
                        if skipped > 0 {
                            cx.emit(PaneEvent::Notice {
                                message: format!(
                                    "{} in this archive could not be shown",
                                    match skipped {
                                        1 => "1 name".to_string(),
                                        n => format!(
                                            "{} names",
                                            crate::notifications::count(n as u64)
                                        ),
                                    }
                                ),
                                problem: false,
                            });
                        }
                    }
                    Err(err) => {
                        // A directory that has gone away is not a state to sit
                        // in: the pane would show a message with no way out but
                        // typing a path. Fall back to the nearest ancestor that
                        // still exists, the parent of a deleted folder, or the
                        // mount point's parent when a volume is unplugged.
                        //
                        // Only when it is *gone*. A directory that exists but
                        // cannot be read is a real, actionable error, and
                        // bouncing to the parent would hide it.
                        //
                        // Only for a real directory, too: an archive that
                        // cannot be read is a thing to say so about, not a
                        // place to quietly climb out of.
                        let fallback = this
                            .dir
                            .disk()
                            .filter(|dir| !dir.is_dir())
                            .and_then(fs::nearest_existing_dir)
                            .filter(|dir| Some(dir.as_path()) != this.dir.disk());

                        if let Some(dir) = fallback {
                            // Informational: nothing failed, the pane
                            // just moved somewhere that still exists.
                            cx.emit(PaneEvent::Notice {
                                message: format!(
                                    "{} is gone. Showing {}.",
                                    this.dir,
                                    dir.display()
                                ),
                                problem: false,
                            });
                            this.navigate_to(Location::Disk(dir), cx);
                            return;
                        }
                        this.entries.clear();
                        this.error = Some(err.to_string());
                        // The failure is a re-read like any other. Without this
                        // the walk kept running over a tree the pane no longer
                        // shows, and its figures came back as current when the
                        // directory did.
                        this.listing_moved += 1;
                        this.directory_read += 1;
                    }
                }
                // Same directory: find the renamed row again. A navigation is
                // different, the same name elsewhere is a different file, so
                // the rename ends with the directory it belonged to.
                if this.loaded_dir.as_ref() == Some(&this.dir) {
                    this.reanchor_rename();
                } else {
                    this.renaming = None;
                }
                this.loaded_dir = Some(this.dir.clone());
                this.restore_selection();
                // Without this the mutation lands but nothing repaints.
                cx.notify();
            });
        }));
    }

    /// Re-list when something other than pane changes this directory.
    ///
    /// Without this the listing goes quietly stale: a file dragged out to
    /// another application, removed in a terminal, or written by a build all
    /// leave the pane showing entries that are no longer there. A file manager
    /// showing yesterday's truth is worse than a slow one.
    fn watch_dir(&mut self, cx: &mut Context<Self>) {
        // The file behind what is being shown, which is the directory itself
        // on disk and the archive file inside one: both are things a change
        // to would make the listing wrong.
        let anchor = self.dir.anchor();
        if self.watch.as_ref().map(|(dir, _)| dir.as_path()) == Some(anchor) {
            return;
        }
        self.watch = None;
        // A directory is watched directly; an archive is watched through the
        // directory holding it. `notify` on a file follows the inode, and
        // anything that rewrites an archive does it by writing a new one and
        // renaming it over the old, which would leave the watch pointing at the
        // file that used to be there. `watch_settings` does the same thing for
        // the same reason.
        //
        // The cost is that any change in that directory wakes this, and a
        // wasted re-list is cheap next to a listing that quietly goes wrong.
        let archive = (!self.dir.is_disk()).then(|| anchor.to_path_buf());
        let dir = match &archive {
            Some(file) => match file.parent() {
                Some(parent) => parent.to_path_buf(),
                None => return,
            },
            None => anchor.to_path_buf(),
        };

        let Some(watch) = crate::config::DirWatch::new(&dir, "directory") else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            loop {
                // A copy or an extract emits one event per file. The settle
                // coalesces the burst, so a long operation re-lists a few
                // times rather than thousands, which matters when the
                // directory is large.
                watch
                    .changed(cx.background_executor(), WATCH_INTERVAL, WATCH_INTERVAL)
                    .await;

                // The arranged contents are remembered per archive, so
                // forgetting them is what makes the re-list read the file again
                // rather than hand back what it said before it changed.
                if let Some(file) = &archive {
                    crate::archive::forget(file);
                }

                let alive = this.update(cx, |this, cx| this.reload_from_watch(cx));
                if alive.is_err() {
                    break;
                }
            }
        });
        self.watch = Some((self.dir.anchor().to_path_buf(), task));
    }

    /// Ask git about this directory off the UI thread.
    ///
    /// `git status` spawns a process and walks the work tree: 142ms in a 17k
    /// file repository, so the listing never waits for it. The names colour in
    /// when the answer arrives. Assigning the task drops any previous one, so a
    /// fast sequence of navigations cannot land a stale answer on the wrong
    /// directory.
    fn reload_git(&mut self, cx: &mut Context<Self>) {
        // `git -C` needs a directory to run in, and a repository cannot be
        // inside an archive anyway.
        let Some(dir) = self.dir.disk().map(Path::to_path_buf) else {
            self.git = crate::git::GitStatuses::new();
            self.git_for = None;
            self.git_watch = None;
            return;
        };
        // Another directory's statuses must not tint this one. The same
        // directory keeps its colours until the answer, so a refresh does not
        // flash every name to plain and back.
        if self.git_for.as_ref() != Some(&dir) {
            self.git = crate::git::GitStatuses::new();
        }
        self.git_task = Some(cx.spawn(async move |this, cx| {
            let (statuses, git_dir) = cx
                .background_spawn({
                    let dir = dir.clone();
                    async move { (crate::git::statuses(&dir), crate::git::git_dir(&dir)) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Empty is an answer too: everything committed, or no
                // repository, and the names go back to plain.
                this.git = statuses;
                this.git_for = Some(dir);
                this.watch_git(git_dir, cx);
                cx.notify();
            });
        }));
    }

    /// Watch the repository's git directory, so a commit, an add, a checkout
    /// or a reset recolours the names. None of them touches the listed
    /// directory, which is all `watch_dir` sees, so without this the colours
    /// stay as they were until the pane is next re-read.
    fn watch_git(&mut self, git_dir: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(git_dir) = git_dir else {
            self.git_watch = None;
            return;
        };
        if self.git_watch.as_ref().map(|(dir, _)| dir) == Some(&git_dir) {
            return;
        }
        // Not recursive: the index, HEAD and the files beside them are what a
        // status depends on, and `objects/` under it churns on every write.
        let Some(watch) = crate::config::DirWatch::new(&git_dir, "git") else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            loop {
                // A commit writes several files in a row. Ask once.
                watch
                    .changed(cx.background_executor(), WATCH_INTERVAL, WATCH_INTERVAL)
                    .await;
                if this.update(cx, |this, cx| this.reload_git(cx)).is_err() {
                    break;
                }
            }
        });
        self.git_watch = Some((git_dir, task));
    }

    /// What is selected and where the cursor is, by key rather than by index.
    ///
    /// Indices name rows; a rebuild that re-sorts names different files with
    /// the same numbers. Anything that replaces `entries` wholesale while the
    /// user is looking at them has to carry the selection across by identity.
    fn held_selection(&self) -> HeldSelection {
        let key_at = |ix: Option<usize>| {
            ix.and_then(|ix| self.entries.get(ix))
                .map(|entry| entry.key().to_path_buf())
        };
        HeldSelection {
            keys: self.selected_keys(),
            cursor: key_at(self.cursor_ix),
            // The anchor too. Carrying the cursor and the selection but not
            // this left a shift-click extending from whatever file had moved
            // into the old index, so the range covered a different span than
            // the one anchored — and the next copy acted on it.
            anchor: key_at(self.anchor_ix),
        }
    }

    /// Put a selection taken by `held_selection` back onto the current rows.
    ///
    /// Rows that are no longer there are dropped rather than approximated: a
    /// selection that quietly moved to a neighbour is worse than one that
    /// shrank, because the next copy or delete acts on it.
    fn put_selection_back(&mut self, held: HeldSelection) {
        if held.keys.is_empty() && held.cursor.is_none() && held.anchor.is_none() {
            return;
        }
        let at = |wanted: &Path| self.entries.iter().position(|e| e.key() == wanted);
        let rows: std::collections::BTreeSet<usize> =
            held.keys.iter().filter_map(|k| at(k)).collect();
        self.selected.set(rows);
        self.cursor_ix = held.cursor.as_deref().and_then(at);
        self.anchor_ix = held.anchor.as_deref().and_then(at);
    }

    /// Re-select `pending_select` against the freshly built listing and scroll
    /// the first survivor into view. Entries that disappeared are dropped.
    /// Read an archive a piece at a time, showing rows as they arrive.
    ///
    /// A zip is read in milliseconds and lands in one piece like a directory
    /// would. A tarball has no index at all, so listing one *is* decompressing
    /// all of it: measured here, twelve seconds for a middling `.tar.bz2` and a
    /// minute for the largest. Waiting for the whole answer would show an empty
    /// pane for a minute, so the rows go up as they are found.
    ///
    /// What a batch may touch is the delicate part, and the answers are the
    /// same ones `set_filter` arrived at for search:
    ///
    /// - `directory_read` stays put, because bumping it restarts the folder
    ///   size walk, and each restart re-eats its debounce so the walk would
    ///   never run at all.
    /// - `restore_selection` runs once at the end. It takes `pending_select`,
    ///   so a second call has nothing left and would clear what the first one
    ///   restored.
    /// - `reanchor_rename` likewise: it closes the rename editor when it cannot
    ///   find the row, and early on it will not be there yet.
    /// - `loaded_dir` is what tells a navigation from a refresh, so it flips
    ///   when the listing is whole and not before.
    fn spawn_archive_read(
        &mut self,
        archive: PathBuf,
        inside: PathBuf,
        cancel: crate::archive::Cancel,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let reading = crate::archive::spawn_read(&archive, cancel);

        cx.spawn(async move |this, cx| {
            let mut members: Vec<crate::archive::Member> = Vec::new();
            // Rebuilt when the count has grown by a quarter rather than on a
            // timer. A rebuild is a pass over everything read so far, so a
            // timer makes that cost O(n) per tick where growth makes the whole
            // read cost O(n) however long it takes.
            let mut arranged_at = 0usize;
            let mut cleared = false;

            loop {
                cx.background_executor().timer(ARCHIVE_POLL).await;

                let batch = reading.drain();
                let finished = reading.is_done();
                members.extend(batch);

                // Rearranged only when there is meaningfully more to arrange,
                // because a rebuild is a pass over everything read so far.
                let grown = members.len() > arranged_at + arranged_at / 4;
                if !grown && !finished {
                    // Nothing new to show, but the read has moved on, and a
                    // figure that stops climbing while it grinds through a
                    // three hundred megabyte member reads as a hang. This is
                    // the common case for a CUDA package: thirty members, and
                    // ten seconds between two of them.
                    let alive = this.update(cx, |this, cx| {
                        this.reading_bytes = Some(reading.bytes());
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                    continue;
                }
                arranged_at = members.len();

                // Cloned because the index owns what it arranges, and the
                // accumulation has to survive for the next rebuild.
                let listing = crate::archive::Listing {
                    members: members.clone(),
                    skipped: reading.skipped(),
                };
                let index = Arc::new(crate::archive::Index::build(listing));

                let alive = this.update(cx, |this, cx| {
                    if let Some(fault) = reading.fault() {
                        this.entries.clear();
                        this.error = Some(fault);
                        this.listing_moved += 1;
                        this.directory_read += 1;
                        this.loaded_dir = Some(this.dir.clone());
                        this.reading_bytes = None;
                        cx.notify();
                        return false;
                    }

                    // Kept every tick, not only at the end: a read of a large
                    // tarball is the case where having the members early is
                    // worth most, and a partial arrangement is a true answer
                    // about the part that has arrived.
                    this.held = Some(HeldIndex {
                        archive: archive.clone(),
                        // On the last tick, the reader has already put an
                        // identical arrangement in the cache — it does that
                        // before saying it is done, on purpose — so this holds
                        // that one instead and the two stop being two.
                        index: if finished {
                            crate::archive::remembered(&archive).unwrap_or_else(|| index.clone())
                        } else {
                            index.clone()
                        },
                    });

                    // Read fresh on every tick, for the same reason the sort
                    // below is: toggling hidden files mid-read must take
                    // effect on the next batch, not wait for a stale value
                    // captured before this task's loop even started.
                    let rows =
                        crate::archive::rows_in(&index, &archive, &inside, this.view.show_hidden);
                    match rows {
                        Some(rows) => {
                            // Rows are about to be replaced and re-sorted
                            // under indices that name positions rather than
                            // files. Held by key across it, because a row
                            // picked while the archive was still being read
                            // otherwise became a different member as later
                            // ones sorted above it — and the copy that
                            // followed took a file nobody had pointed at.
                            let held = this.held_selection();
                            // A search is standing in for the listing, so what
                            // is on screen is results and must not be replaced
                            // by the directory. Re-running it against what has
                            // arrived is what makes results grow with the read
                            // rather than being wiped by it every eighty
                            // milliseconds — which is what used to happen, and
                            // left the footer counting matches for a listing
                            // nobody had searched.
                            match this.search.as_ref().map(|s| s.query.clone()) {
                                Some(query) => {
                                    let capped = this.filter_archive(&query);
                                    if let Some(SearchWork::Filtered { capped: at }) =
                                        this.search.as_mut().map(|s| &mut s.work)
                                    {
                                        *at = capped;
                                    }
                                }
                                None => {
                                    // Sorted here rather than above, so a
                                    // search does not pay for a listing it is
                                    // about to throw away on every tick of a
                                    // slow read. Read fresh either way: a
                                    // header clicked mid-read updates
                                    // `view.sort` right now, and a copy taken
                                    // at the top of this task would overwrite
                                    // that on the next batch.
                                    let mut entries = rows.entries;
                                    fs::sort_entries(
                                        &mut entries,
                                        this.view.sort,
                                        this.view.folders_first,
                                    );
                                    this.entries = entries;
                                }
                            }
                            this.put_selection_back(held);
                            this.error = None;
                            if finished && rows.skipped > 0 {
                                cx.emit(PaneEvent::Notice {
                                    message: format!(
                                        "{} in this archive could not be shown",
                                        match rows.skipped {
                                            1 => "1 name".to_string(),
                                            n => format!(
                                                "{} names",
                                                crate::notifications::count(n as u64)
                                            ),
                                        }
                                    ),
                                    problem: false,
                                });
                            }
                        }
                        // No such directory. Only worth saying once the read is
                        // whole: until then it may simply not have arrived.
                        None if finished => {
                            this.entries.clear();
                            this.error = Some("that folder is not in this archive".to_string());
                        }
                        None => {}
                    }

                    // The rows are a different directory's now, so the
                    // selection that was on the old ones means nothing. A
                    // refresh of the same archive put its keys in
                    // `pending_select`, and `restore_selection` below puts them
                    // back once the listing is whole.
                    if !cleared {
                        this.clear_cursor();
                        cleared = true;
                    }
                    this.listing_moved += 1;
                    this.reading_bytes = (!finished).then(|| reading.bytes());

                    if finished {
                        // The tail a directory read runs too, once.
                        this.directory_read += 1;
                        if this.loaded_dir.as_ref() == Some(&this.dir) {
                            this.reanchor_rename();
                        } else {
                            this.renaming = None;
                        }
                        this.loaded_dir = Some(this.dir.clone());
                        // `restore_selection` clears whatever is selected and
                        // puts back only what it was asked to, so with nothing
                        // pending it wipes the selection. That is right when
                        // the listing is arriving for the first time and wrong
                        // here: the rows have been on screen for the whole
                        // read and somebody may have picked one, so what they
                        // picked *is* what to restore.
                        if this.pending_select.is_empty() {
                            this.pending_select = this.selected_keys();
                        }
                        this.restore_selection();
                    }
                    cx.notify();
                    !finished
                });

                if !alive.unwrap_or(false) {
                    break;
                }
            }
        })
    }

    fn restore_selection(&mut self) {
        let place = self.pending_place.take();
        let mut wanted = std::mem::take(&mut self.pending_select);
        if wanted.is_empty()
            && let Some(place) = &place
        {
            wanted = place.selected.clone();
        }
        // A set, not the Vec: this runs once per entry, so a linear scan makes
        // restoring a large selection quadratic: measured at 11 seconds for
        // 100k rows selected, on the foreground executor.
        let wanted: std::collections::HashSet<PathBuf> = wanted.into_iter().collect();
        self.selected.clear();
        self.anchor_ix = None;
        self.cursor_ix = None;
        if let Some(place) = place {
            for (ix, entry) in self.entries.iter().enumerate() {
                if wanted.contains(entry.key()) {
                    self.selected.insert(ix);
                }
            }
            self.return_to(&place);
            return;
        }
        if wanted.is_empty() {
            self.scroll_to(0, Reveal::Top);
            return;
        }
        for (ix, entry) in self.entries.iter().enumerate() {
            if wanted.contains(entry.key()) {
                self.selected.insert(ix);
            }
        }
        self.anchor_ix = self.selected.first();
        self.cursor_ix = self.anchor_ix;
        match self.anchor_ix {
            Some(ix) => self.scroll_to(ix, Reveal::Nearest),
            None => self.scroll_to(0, Reveal::Top),
        }
    }

    /// Header click: clicking the active column reverses it, a new column adopts
    /// its natural starting direction. The header carries a chevron, so the
    /// reversal is visible where it happens.
    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let dir = if self.view.sort.key == key {
            self.view.sort.dir.toggled()
        } else {
            Self::natural_direction(key)
        };
        self.set_sort(Sort { key, dir }, cx);
    }

    /// Choose a sort key without reversing. The view menu uses this: its check
    /// mark cannot show a direction, so picking the key you already have must
    /// be a no-op rather than a silent reversal.
    fn select_sort_key(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.view.sort.key == key {
            return;
        }
        self.set_sort(
            Sort {
                key,
                dir: Self::natural_direction(key),
            },
            cx,
        );
    }

    /// Time starts newest-first; everything else starts ascending. Choosing
    /// "Modified" and getting 2019 first would be nobody's intent.
    fn natural_direction(key: SortKey) -> SortDir {
        match key {
            SortKey::Modified => SortDir::Descending,
            _ => SortDir::Ascending,
        }
    }

    fn set_sort(&mut self, sort: Sort, cx: &mut Context<Self>) {
        if self.view.sort == sort {
            return;
        }
        self.view.sort = sort;
        self.apply_sort(cx);
    }

    fn reverse_sort(&mut self, _: &ReverseSort, _w: &mut Window, cx: &mut Context<Self>) {
        let sort = Sort {
            key: self.view.sort.key,
            dir: self.view.sort.dir.toggled(),
        };
        self.set_sort(sort, cx);
    }

    /// Re-order the listing already in memory, without re-reading the directory.
    ///
    /// Measured at ~90-150ms for 100k entries in a debug build, which is well past the
    /// frame budget, so it runs on the background executor. The current list stays on
    /// screen until the sorted one arrives rather than blanking.
    fn apply_sort(&mut self, cx: &mut Context<Self>) {
        cx.emit(PaneEvent::ViewChanged);
        if self.pending_select.is_empty() {
            self.pending_select = self.selected_keys();
        }
        let sort = self.view.sort;
        let folders_first = self.view.folders_first;
        let mut entries = self.entries.clone();
        // Sorting by size with the sizes already counted, rather than treating
        // every folder as having none and waiting for the walk to settle before
        // the order means anything.
        let sizes = self.footer.settled.clone();
        // How much was known when the snapshot was taken. The sort runs off the
        // foreground executor, and a walk settling during that hop leaves this
        // result describing an order computed from fewer sizes than the pane
        // now holds.
        let knew = sizes.len();

        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn(async move {
                    fs::sort_entries_by(&mut entries, sort, folders_first, |entry| {
                        entry.size.or_else(|| sizes.get(entry.key()).copied())
                    });
                    entries
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.entries = entries;
                this.listing_moved += 1;
                // Whatever was still being counted can still reorder the rows
                // once it lands; this sort used only what was known so far.
                this.footer.resorted = false;
                // The rows moved, so re-find them by name rather than index.
                this.renaming = None;
                this.restore_selection();
                this.scroll_to(0, Reveal::Top);
                // More is known now than when this sort was handed off, so the
                // order it computed is already out of date. The walk's own
                // re-sort may also have run and been overwritten by the line
                // above, and its guard would have stopped it running twice.
                if this.footer.settled.len() != knew {
                    this.resort_for_sizes(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn navigate_to(&mut self, dir: Location, cx: &mut Context<Self>) {
        if dir == self.dir {
            return;
        }
        let leaving = self.current_place();
        self.history.push(dir.clone(), leaving);
        self.load_dir_only(dir, cx);
    }

    /// Where this listing is being left from, for Back to return to.
    fn current_place(&self) -> Place {
        let key = |ix: usize| self.entries.get(ix).map(|e| e.key().to_path_buf());
        Place {
            cursor: self.cursor_ix.and_then(key),
            selected: self.selected_keys(),
            top: key(self.top_row()),
        }
    }

    /// The first row in view. From the scroll offset and the fixed row
    /// height: gpui's own `logical_scroll_top_index` exists only in its test
    /// builds.
    fn top_row(&self) -> usize {
        if self.view.layout == fs::Layout::Grid {
            return self.grid_top();
        }
        let offset = self.scroll.0.borrow().base_handle.offset();
        (-offset.y / px(ROW_HEIGHT)).max(0.) as usize
    }

    /// Stand where `place` says, in the listing that has just landed: the
    /// cursor on the row it was on, and the same row at the top, which is
    /// what a browser does with a page's scroll position on Back.
    ///
    /// The top row is put back only while the cursor stays in view under it.
    /// A pane resized while it was away could otherwise restore a scroll that
    /// hides the very row the cursor is on, so then the cursor wins.
    fn return_to(&mut self, place: &Place) {
        let find = |key: &Path| self.entries.iter().position(|e| e.key() == key);
        let cursor = place
            .cursor
            .as_deref()
            .and_then(find)
            .or_else(|| self.selected.first());
        let top = place.top.as_deref().and_then(find);
        self.anchor_ix = cursor;
        self.cursor_ix = cursor;
        let in_view = |top: usize, row: usize| match self.view.layout {
            fs::Layout::List => row >= top && row < top + self.visible_rows(),
            fs::Layout::Grid => self.grid_in_view(top, row),
        };
        match (top, cursor) {
            (Some(top), Some(row)) if in_view(top, row) => {
                self.scroll_to(top, Reveal::TopEvenPastTheEnd)
            }
            (Some(top), None) => self.scroll_to(top, Reveal::TopEvenPastTheEnd),
            (_, Some(row)) => self.scroll_to(row, Reveal::Nearest),
            (None, None) => self.scroll_to(0, Reveal::Top),
        }
    }

    /// Whole rows the viewport holds, from the last layout. Measured against
    /// the fixed row height rather than the list's contents, which still
    /// describe the listing that was there before this one.
    fn visible_rows(&self) -> usize {
        match self.scroll.0.borrow().last_item_size {
            Some(size) => (size.item.height / px(ROW_HEIGHT)) as usize,
            // No layout yet, so nothing to say the cursor would be hidden.
            None => usize::MAX / 2,
        }
    }

    /// Change directory without touching history, the back/forward path.
    fn load_dir_only(&mut self, dir: Location, cx: &mut Context<Self>) {
        self.dir = dir;
        self.reload(cx);
        cx.notify();
    }

    fn nav_back(&mut self, _: &NavBack, _window: &mut Window, cx: &mut Context<Self>) {
        let leaving = self.current_place();
        if let Some((dir, place)) = self.history.back(leaving) {
            let dir = dir.clone();
            self.pending_place = place.cloned();
            self.load_dir_only(dir, cx);
        }
    }

    fn nav_forward(&mut self, _: &NavForward, _window: &mut Window, cx: &mut Context<Self>) {
        let leaving = self.current_place();
        if let Some((dir, place)) = self.history.forward(leaving) {
            let dir = dir.clone();
            self.pending_place = place.cloned();
            self.load_dir_only(dir, cx);
        }
    }

    fn rename_selected(&mut self, _: &RenameSelected, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.cursor_ix.filter(|&ix| ix < self.entries.len()) else {
            return;
        };
        self.start_rename(ix, window, cx);
    }

    fn start_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let name = entry.name.clone();
        // The anchor `reanchor_rename` looks the row back up by, so a key and
        // not a path: it is only ever compared, never opened.
        let entry_path = entry.key().to_path_buf();
        let selection = fs::stem_range(&name);
        let editor = cx.new(|cx| PathEditor::new_with_selection(name, selection, window, cx));

        cx.subscribe_in(
            &editor,
            window,
            |this, editor, event, window, cx| match event {
                PathEditorEvent::Committed(text) => {
                    let Some(ix) = this.renaming.as_ref().map(|r| r.ix) else {
                        return;
                    };
                    match this.commit_rename(ix, text) {
                        Ok(renamed) => {
                            this.renaming = None;
                            this.pending_select = vec![renamed];
                            window.focus(&this.focus_handle, cx);
                            this.reload(cx);
                        }
                        Err(reason) => {
                            // The field turns red; the reason is available for a
                            // tooltip or status line once there is somewhere to put it.
                            let _ = reason;
                            editor.update(cx, |editor, cx| {
                                editor.error = true;
                                cx.notify();
                            });
                        }
                    }
                }
                PathEditorEvent::Edited => {}
                // A rename field owns the row it sits on, so losing focus and
                // pressing Escape both mean the same thing: leave the name
                // alone.
                PathEditorEvent::Cancelled | PathEditorEvent::Blurred => {
                    this.renaming = None;
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }
            },
        )
        .detach();

        let editor_focus = editor.focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                window.focus(&editor_focus, cx);
            });
        });
        self.renaming = Some(Renaming {
            path: entry_path,
            ix,
            editor,
        });
        cx.notify();
    }

    /// Returns the new name, or why the rename was refused. `RENAME_NOREPLACE`
    /// refuses to clobber an existing entry; filesystems without renameat2
    /// support fall back to an existence check plus a plain rename.
    fn commit_rename(&self, ix: usize, new_name: &str) -> Result<PathBuf, String> {
        let entry = self
            .entries
            .get(ix)
            .ok_or_else(|| "the entry is gone".to_string())?;
        // Nothing to rename: a row that is not a file on the disk is a member
        // of an archive, and renaming one means rewriting the archive.
        let path = entry
            .on_disk()
            .ok_or_else(|| "there is no file here to rename".to_string())?;
        let new_name = new_name.trim();
        if let Some(problem) = fs::name_problem(new_name) {
            return Err(problem.to_string());
        }
        if new_name == entry.name {
            return Ok(path.to_path_buf());
        }
        // The entry's own directory, which is the pane's only when the listing
        // is a plain one: a search result lives somewhere below it, and joining
        // `self.dir` would move the file up to the search root as a side effect
        // of renaming it.
        let target = path
            .parent()
            .or_else(|| self.dir.disk())
            .unwrap_or(Path::new("/"))
            .join(new_name);
        hoja_transfer::rename_no_replace(path, &target)
            .map(|()| target.clone())
            .map_err(|err| err.to_string())
    }

    fn edit_path(&mut self, _: &EditPath, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(window, cx);
    }

    fn start_search(&mut self, _: &StartSearch, window: &mut Window, cx: &mut Context<Self>) {
        // Reopening keeps whatever is already typed, so ctrl-f twice does not
        // silently throw the search away.
        let initial = self
            .search
            .as_ref()
            .map(|s| s.query.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            PathEditor::new(initial, window, cx)
                .with_placeholder("Search this folder")
                // So the arrows can be given back; see the bindings in `main`.
                .also_in("SearchBar")
        });

        cx.subscribe_in(
            &editor,
            window,
            |this, editor, event, window, cx| match event {
                PathEditorEvent::Edited => {}
                // Enter opens what is chosen.
                //
                // It used to mean "hand the keyboard back", because the arrows
                // could not work while the field held it and that was the only
                // way to reach the results at all. They work from inside the
                // field now, which left Enter meaning nothing at the end of the
                // one journey it exists for: arrow to a result, press Enter,
                // watch the field close, press Enter again.
                //
                // Only while searching. With no query there is nothing the
                // field has chosen, and closing it is the whole of what Enter
                // should do rather than opening whatever the listing was on.
                PathEditorEvent::Committed(_) => {
                    this.path_editor = None;
                    window.focus(&this.focus_handle, cx);
                    if this.searching()
                        && let Some(ix) = this.cursor_ix
                    {
                        this.activate(ix, cx);
                    }
                    cx.notify();
                }
                PathEditorEvent::Cancelled => {
                    this.end_search(window, cx);
                    cx.notify();
                }
                // Focus left the field, which for a search is not a cancellation.
                // The results *are* the pane's content, so clicking one is how
                // they are meant to be used, and ending the search there threw
                // away the thing the click was reaching for. This is where Enter
                // already arrives, minus the focus grab: focus has gone wherever
                // the click landed, and taking it back would fight that.
                //
                // Unless it has gone nowhere. The window losing focus blurs the
                // field while leaving the keyboard exactly where it was, on the
                // field this is about to drop, and a focus pointing at an element
                // that no longer exists is a focus path with no `DirPane` in it:
                // every pane binding stays masked by a field that has gone. The
                // pane came back from an alt-tab with a dead keyboard.
                PathEditorEvent::Blurred => {
                    let stranded = editor.focus_handle(cx).is_focused(window);
                    this.path_editor = None;
                    if stranded {
                        window.focus(&this.focus_handle, cx);
                    }
                    cx.notify();
                }
            },
        )
        .detach();

        // Live: the pane re-filters on every keystroke rather than on enter.
        cx.observe(&editor, |this, editor, cx| {
            let query = editor.read(cx).text().to_string();
            this.set_filter(Some(query), cx);
        })
        .detach();

        window.focus(&editor.focus_handle(cx), cx);
        self.path_editor = Some((BarMode::Search, editor));
        cx.notify();
    }

    /// Start, replace, or end a search of everything under this directory.
    fn set_filter(&mut self, query: Option<String>, cx: &mut Context<Self>) {
        let query = query.filter(|q| !q.is_empty());
        if query.as_deref() == self.search.as_ref().map(|s| s.query.as_str()) {
            return;
        }

        // Dropping the old search stops its walk, so a new keystroke abandons
        // the previous query rather than racing it into the same list, and
        // hands the listing back when the last query goes.
        let previous = self.search.take();
        let Some(query) = query else {
            if let Some(previous) = previous {
                // Put the saved listing back for the frame, then re-read it.
                // The watcher stands down while a search runs, so this listing
                // is as old as the search: showing it immediately keeps the
                // pane from blanking, and the read behind it makes it true.
                // Rebuilt from the arrangement where there is one, rather
                // than put back from the copy taken when the search began:
                // inside an archive that copy can be older than the read is,
                // since a search started mid-read saved whatever had arrived
                // by then and the read has gone on since.
                self.entries = previous.listing;
                self.rebuild_from_held();
                self.listing_moved += 1;
                self.clear_cursor();
                self.restore_selection();
                // Except in an archive whose arrangement this pane is holding,
                // where the listing put back above is already the truth and
                // re-reading would be worse than pointless. `reload` cancels
                // the read in flight and starts another from nothing, so
                // escaping a search begun during a slow one threw away the
                // decompression so far and began it again — `remember` only
                // runs on a clean finish, so none of that work was kept.
                //
                // Falling through when there is no arrangement is what keeps a
                // broken archive honest: nothing was ever built for it, so the
                // read has to run again to put its error back.
                if !self.holding_this_archive() {
                    self.reload(cx);
                }
                cx.notify();
                return;
            }
            self.clear_cursor();
            self.restore_selection();
            cx.notify();
            return;
        };

        // The listing steps aside for the first query and stays aside while
        // later ones replace each other.
        let listing = match previous {
            Some(previous) => previous.listing,
            None => std::mem::take(&mut self.entries),
        };
        self.entries.clear();
        self.listing_moved += 1;
        self.clear_cursor();
        self.error = None;

        // The walker reads directories off the disk, so there is nothing for
        // it to walk inside an archive. The arrangement the pane is holding
        // knows every member, though, so this searches that instead: no
        // thread, no debounce, no cancellation to get right, and done before
        // this returns. `end_search`'s restore path needs nothing extra,
        // since it puts `previous.listing` back.
        let Some(dir) = self.dir.disk().map(Path::to_path_buf) else {
            let capped = self.filter_archive(&query);
            self.listing_moved += 1;
            // Aim at the first hit as soon as there is one, matching the
            // disk-search behaviour below: enter opens it without arrowing
            // down first.
            if self.cursor_ix.is_none() && !self.entries.is_empty() {
                self.selected.only(0);
                self.place_cursor(0);
            }
            self.search = Some(ActiveSearch {
                query,
                listing,
                work: SearchWork::Filtered { capped },
            });
            cx.notify();
            return;
        };
        let show_hidden = self.view.show_hidden;
        let spawn_query = query.clone();

        // Results arrive over the life of the walk, so the pane collects them
        // on a timer rather than waiting for the end.
        let poll = cx.spawn(async move |this, cx| {
            // Settle before walking: without this every keystroke spawned a
            // thread and re-walked the whole tree.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            if this
                .update(cx, |this, _| {
                    if let Some(SearchWork::Walk { handle, .. }) =
                        this.search.as_mut().map(|s| &mut s.work)
                    {
                        *handle = Some(crate::search::spawn(dir, spawn_query, show_hidden));
                    }
                })
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(60))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    let Some(SearchWork::Walk {
                        handle: Some(handle),
                        ..
                    }) = this.search.as_ref().map(|s| &s.work)
                    else {
                        return false;
                    };
                    let batch = handle.drain();
                    let finished = handle.is_done();
                    if !batch.is_empty() {
                        this.entries.extend(batch);
                        this.listing_moved += 1;
                        // Aim at the first hit as soon as there is one, so
                        // enter opens it without arrowing down first. Only
                        // while nothing is chosen: later batches must not
                        // yank the selection off what you picked.
                        if this.cursor_ix.is_none() && !this.entries.is_empty() {
                            this.selected.only(0);
                            this.place_cursor(0);
                        }
                        cx.notify();
                    }
                    if finished {
                        cx.notify();
                    }
                    !finished
                });
                if !keep_going.unwrap_or(false) {
                    break;
                }
            }
        });

        self.search = Some(ActiveSearch {
            query,
            listing,
            work: SearchWork::Walk { handle: None, poll },
        });
        cx.notify();
    }

    /// Forget which row is current. Called wherever the rows themselves change
    /// out from under the indices.
    fn clear_cursor(&mut self) {
        self.selected.clear();
        self.anchor_ix = None;
        self.cursor_ix = None;
    }

    /// Point an open rename at the row its entry now occupies, or end it if the
    /// entry is gone: renamed or deleted by something else while it was open.
    fn reanchor_rename(&mut self) {
        let Some(path) = self.renaming.as_ref().map(|r| r.path.clone()) else {
            return;
        };
        match self.entries.iter().position(|entry| entry.key() == path) {
            Some(ix) => {
                if let Some(renaming) = self.renaming.as_mut() {
                    renaming.ix = ix;
                }
            }
            None => self.renaming = None,
        }
    }

    /// Bring the footer and the column in line with what is on screen.
    ///
    /// Called from `render`, which is the one place guaranteed to run after any
    /// of its inputs move: every path that changes the selection, replaces the
    /// rows, or changes directory already notifies. A call at each of the eight
    /// selection sites instead is the arrangement that goes stale the first time
    /// a ninth is added.
    ///
    /// Two comparisons and out on the frames where nothing moved, which is most
    /// of them. It must not notify: that would be asking for the frame it is
    /// already inside.
    fn sync_footer(&mut self, cx: &mut Context<Self>) {
        // The walk belongs to the listing. Moving the selection re-reads the
        // buckets but never restarts them, which is what makes selecting a
        // folder free, the number is already there, or already coming.
        let moved_directory = self.footer.dir.as_ref() != Some(&self.dir);
        let reread = self.footer.read != self.directory_read || moved_directory;
        let relisted = self.footer.listing != self.listing_moved || reread;
        if reread {
            self.footer.read = self.directory_read;
            // Only a read from disk, never a re-order. Comparing the *set* of
            // folder paths instead looked equivalent and was not: a sort and a
            // paste into a subfolder both leave that set identical, so one of
            // them kept sizes that had just become wrong.
            //
            // Nothing tells us a subtree changed. The pane watches its own
            // directory and not below it, so a folder's size is only ever as
            // fresh as the moment it was measured; re-reading the directory is
            // the one honest cue there is that it might not be.
            self.footer.dir = Some(self.dir.clone());
            if moved_directory {
                // Different paths entirely, so the old figures answer nothing
                // and would only grow the map.
                self.footer.settled.clear();
            }
            self.restart_walk(cx);
        }
        if relisted {
            self.footer.listing = self.listing_moved;
        }
        if relisted
            || self.footer.selection != self.selected.revision()
            || self.footer.columns != self.view.columns
        {
            self.footer.selection = self.selected.revision();
            self.footer.columns = self.view.columns;
            self.footer.summary = self.summarise();
            self.footer.text = self.footer_text();
        }
    }

    /// Abandon any walk and start one for the rows now on screen.
    fn restart_walk(&mut self, cx: &mut Context<Self>) {
        // Both go, and in either order: the handle stops the threads, the task
        // stops the poll and any walk the debounce had not let it start.
        self.footer.walk = None;
        self.footer.poll = None;
        self.footer.resorted = false;
        // `settled` is deliberately kept. It is the last figure measured for
        // each path, and the new walk overwrites each one as it finishes, so a
        // directory that keeps receiving folders shows slightly old numbers
        // rather than emptying its Size column back to placeholders every time
        // the watcher fires. `sync_footer` clears it when the paths change.

        // A search's rows come from directories this pane is not showing, and an
        // unreadable one has already said so in the body. Neither is worth a
        // walk.
        //
        // And only the folders that need one: a folder inside an archive
        // already carries its exact total, and there is no path under it for
        // the walker to read anyway. `on_disk` is what says so. Without it the
        // walker would fail to `stat` each root, treat it as finished at zero,
        // and every folder in the archive would print a confident `0 B` that
        // nothing would ever correct.
        self.footer.roots = if self.searching() || self.error.is_some() {
            HashMap::new()
        } else {
            self.entries
                .iter()
                .filter(|entry| entry.is_dir && entry.size.is_none())
                .filter_map(|entry| entry.on_disk().map(Path::to_path_buf))
                .enumerate()
                .map(|(root, path)| (path, root))
                .collect()
        };
        if !self.footer.roots.is_empty() {
            self.footer.poll = Some(Self::spawn_measure_poll(cx));
        }
    }

    fn summarise(&self) -> fs::Summary {
        if self.searching() || self.error.is_some() {
            fs::Summary::default()
        } else if self.selected.is_empty() {
            fs::summarise_dir(&self.entries)
        } else {
            fs::summarise_selection(&self.entries, &self.selected, self.view.columns)
        }
    }

    /// What the walk has counted for one row, and whether that figure is final.
    ///
    /// Two hash lookups rather than a scan: the column asks once per visible row
    /// per frame, and a directory of five thousand folders would otherwise make
    /// that quadratic.
    fn bucket(&self, entry_ix: usize) -> Option<(u64, bool)> {
        let entry = self.entries.get(entry_ix)?;
        // A figure that is already final, and immune to any re-sort.
        if let Some(bytes) = self.footer.settled.get(entry.key()) {
            return Some((*bytes, true));
        }
        let walk = self.footer.walk.as_ref()?;
        let root = *self.footer.roots.get(entry.key())?;
        Some((walk.bytes(root), walk.settled(root)))
    }

    /// Move whatever the walk has finished into the path-keyed map.
    fn harvest_settled(&mut self) {
        let Some(walk) = self.footer.walk.as_ref() else {
            return;
        };
        let finished: Vec<(PathBuf, u64)> = self
            .footer
            .roots
            .iter()
            .filter(|(_, root)| walk.settled(**root))
            .map(|(path, root)| (path.clone(), walk.bytes(*root)))
            .collect();
        for (path, bytes) in finished {
            self.footer.settled.insert(path, bytes);
        }
    }

    /// The one re-sort a settled walk earns.
    ///
    /// Only when the Size column is the sort key: by name, kind or date a
    /// folder gaining a size changes nothing, so most of the time the listing
    /// never moves at all. Once per listing, guarded, so a settled walk cannot
    /// reorder the rows twice.
    ///
    /// Not through `apply_sort`, which scrolls to the top and closes an open
    /// rename. This keeps the selection and lets `restore_selection` scroll
    /// back to it, so the view follows what you had rather than jumping away.
    ///
    /// Keeping the rename open is what makes `reanchor_rename` below mandatory.
    /// `Renaming` holds a row index beside its path, and the commit path renames
    /// whatever sits at that index: without the re-anchor, a walk settling while
    /// someone was typing a new name renamed a different file entirely.
    fn resort_for_sizes(&mut self, cx: &mut Context<Self>) {
        if self.footer.resorted
            || self.view.sort.key != SortKey::Size
            || self.footer.settled.is_empty()
        {
            return;
        }
        self.footer.resorted = true;

        self.pending_select = self.selected_keys();
        let sizes = std::mem::take(&mut self.footer.settled);
        let mut entries = std::mem::take(&mut self.entries);
        fs::sort_entries_by(
            &mut entries,
            self.view.sort,
            self.view.folders_first,
            |entry| entry.size.or_else(|| sizes.get(entry.key()).copied()),
        );
        self.entries = entries;
        self.footer.settled = sizes;
        // Deliberately not `listing_moved`: the rows are the same rows in a
        // different order, and bumping it would throw away the finished walk
        // and start it again.
        self.reanchor_rename();
        self.restore_selection();
        cx.notify();
    }

    /// Whether this row is a folder the walk is still counting.
    ///
    /// A directory the walk covers but has not finished, which is the only
    /// state the Size column has nothing true to print. Distinct from a row
    /// with no walk behind it at all, in a search or an unreadable directory,
    /// where a placeholder would be promising a number that is not coming.
    fn counting_size(&self, entry_ix: usize) -> bool {
        let Some(entry) = self.entries.get(entry_ix) else {
            return false;
        };
        entry.is_dir
            && !self.footer.settled.contains_key(entry.key())
            && self.footer.roots.contains_key(entry.key())
    }

    /// A folder's size for the Size column: only once it is final, so a cell
    /// never shows a number that is still climbing.
    fn folder_bytes(&self, entry_ix: usize) -> Option<u64> {
        let (bytes, settled) = self.bucket(entry_ix)?;
        settled.then_some(bytes)
    }

    /// The line, against whatever the walk has counted so far.
    ///
    /// The footer *does* climb, unlike the column: one number moving at the
    /// bottom of the pane reads as progress, where a listing full of them reads
    /// as churn.
    fn footer_text(&self) -> String {
        let mut walked = 0;
        let mut settled = true;
        for row in &self.footer.summary.rows {
            match self.bucket(*row) {
                Some((bytes, done)) => {
                    walked += bytes;
                    settled &= done;
                }
                // The debounce has not elapsed, so there is no walk yet.
                None => settled = false,
            }
        }
        fs::compose(&self.footer.summary, walked, settled)
    }

    /// Wait out the debounce, start the walk, then feed the footer until it ends.
    ///
    /// The task carries nothing of its own: it reads the roots out of the pane
    /// when the timer fires, and the total out of whichever handle the pane owns
    /// at each tick. So there is no captured directory and no captured selection
    /// for a late result to land on, and no generation counter is needed to
    /// notice one: replacing `footer.poll` drops this future at its `await`,
    /// and a future that never resumes cannot install anything.
    fn spawn_measure_poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MEASURE_DEBOUNCE).await;

            let started = this.update(cx, |this, cx| {
                // Read out of the pane when the timer fires, not captured when
                // it was set: the rows may have been replaced in the meantime.
                let mut paths = vec![PathBuf::new(); this.footer.roots.len()];
                for (path, root) in &this.footer.roots {
                    paths[*root] = path.clone();
                }
                if paths.is_empty() {
                    return false;
                }
                this.footer.walk = Some(crate::measure::spawn(paths));
                this.footer.text = this.footer_text();
                cx.notify();
                true
            });
            if !started.unwrap_or(false) {
                return;
            }

            loop {
                cx.background_executor().timer(MEASURE_POLL).await;
                let running = this.update(cx, |this, cx| {
                    let Some(walk) = this.footer.walk.as_ref() else {
                        return false;
                    };
                    let settled = walk.is_done();
                    let text = this.footer_text();
                    // A cell that has just settled changes the column even when
                    // the footer's own digits have not moved, so the frame is
                    // asked for on either.
                    let moved = this.footer.text != text;
                    if moved {
                        this.footer.text = text;
                    }
                    if moved || settled {
                        cx.notify();
                    }
                    this.harvest_settled();
                    if settled {
                        this.resort_for_sizes(cx);
                    }
                    !settled
                });
                if !running.unwrap_or(false) {
                    break;
                }
            }
        })
    }

    /// Whether a search is running or has results on screen.
    fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Leave search mode from wherever: escape, the toolbar button, or the
    /// field being cancelled. Each used to clear a different subset.
    /// Leave the search field for the results, and keep them.
    ///
    /// Enter opens the result it is on and Escape ends the search, so without
    /// this the keyboard had no way to the results themselves: the arrows
    /// move through them from inside the field, but copy, cut, delete and
    /// rename are the listing's, and the field held the keyboard. This is what
    /// a click on a result already does (see the `Blurred` arm in
    /// `start_search`), without the click.
    fn focus_results(&mut self, _: &FocusResults, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((BarMode::Search, _)) = &self.path_editor {
            self.path_editor = None;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn end_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((BarMode::Search, _)) = &self.path_editor {
            self.path_editor = None;
        }
        self.set_filter(None, cx);
        window.focus(&self.focus_handle, cx);
    }

    /// Replace the rows with everything under this directory matching
    /// `query`, and say whether the cap cut it short.
    ///
    /// With no arrangement there is nothing to search and the rows stay empty
    /// until the next read tick fills them in. That is a fraction of a second
    /// on entering an archive and forever on one that failed to read, which is
    /// right in both cases: there is nothing in a broken archive to find.
    fn filter_archive(&mut self, query: &str) -> bool {
        let Location::Archive { archive, inside } = &self.dir else {
            return false;
        };
        let Some(held) = self.held.as_ref().filter(|h| h.archive == *archive) else {
            return false;
        };
        let found =
            crate::archive::search_in(&held.index, archive, inside, query, self.view.show_hidden);
        self.entries = found.entries;
        // Sorted, unlike the disk search, which leaves results in the order
        // they arrive. That is right there and wrong here: results stream in
        // over the life of a walk, so re-sorting would make rows jump under
        // the cursor, where these are built whole in one pass. Leaving them
        // unsorted would also make an archive the one place a column header
        // does nothing.
        fs::sort_entries(&mut self.entries, self.view.sort, self.view.folders_first);
        found.capped
    }

    /// Put the rows of this directory back from the arrangement being held.
    ///
    /// A no-op anywhere there is no arrangement, which leaves the caller's own
    /// listing in place.
    fn rebuild_from_held(&mut self) {
        let Location::Archive { archive, inside } = &self.dir else {
            return;
        };
        let Some(held) = self.held.as_ref().filter(|h| h.archive == *archive) else {
            return;
        };
        let Some(rows) =
            crate::archive::rows_in(&held.index, archive, inside, self.view.show_hidden)
        else {
            return;
        };
        let mut entries = rows.entries;
        fs::sort_entries(&mut entries, self.view.sort, self.view.folders_first);
        self.entries = entries;
    }

    /// Whether this pane holds an arrangement of the archive it is in.
    ///
    /// The archive is checked, not merely the presence of one: a pane that has
    /// moved on still holds the last one until its next read replaces it.
    fn holding_this_archive(&self) -> bool {
        match (&self.dir, &self.held) {
            (Location::Archive { archive, .. }, Some(held)) => held.archive == *archive,
            _ => false,
        }
    }

    /// A word about the results, since the listing is no longer this directory.
    /// Shown in this pane's own footer: a search belongs to the pane running it,
    /// and in the window-wide strip only the active pane's was ever visible.
    pub fn search_status(&self) -> Option<String> {
        let search = self.search.as_ref()?;
        let found = self.entries.len();
        let counted = || match found {
            0 => "no matches".to_string(),
            1 => "1 match".to_string(),
            n => format!("{n} matches"),
        };
        match &search.work {
            // Nothing is still arriving, so there is no "searching…" to
            // report: only how many there were.
            // Still arriving, so the count is "so far" rather than the
            // answer, and saying "3 matches" over an archive that is still
            // being read would be a number that quietly changes.
            SearchWork::Filtered { .. } if self.reading_bytes.is_some() => {
                Some(format!("searching… {found}"))
            }
            SearchWork::Filtered { capped: true } => Some(format!("first {found} matches")),
            SearchWork::Filtered { capped: false } => Some(counted()),
            // No handle yet means the debounce has not elapsed.
            SearchWork::Walk { handle: None, .. } => Some("searching…".to_string()),
            SearchWork::Walk {
                handle: Some(handle),
                ..
            } => Some(if !handle.is_done() {
                format!("searching… {found}")
            } else if handle.hit_cap() {
                format!("first {found} matches")
            } else {
                counted()
            }),
        }
    }

    /// The line the footer actually shows.
    ///
    /// Resolved in one place because the probe has to report what a person is
    /// reading, and `footer.text` is only the settled form of it: during a
    /// search or an archive read the line on screen is a different one, and a
    /// test asserting on the stale text would be asserting on nothing.
    pub fn footer_line(&self) -> String {
        self.search_status()
            .or_else(|| self.reading_status())
            .unwrap_or_else(|| self.footer.text.clone())
    }

    /// The footer line while an archive is being read, which for a tarball can
    /// be the best part of a minute.
    ///
    /// Says the same two things the settled line does, so the numbers do not
    /// jump around when it lands: what is here, and how much of it. The
    /// ellipsis marks them as still climbing, which is the shape the job strip
    /// already uses for a total it does not have yet.
    fn reading_status(&self) -> Option<String> {
        let bytes = self.reading_bytes?;
        Some(format!(
            "reading… {} · {}",
            match self.entries.len() {
                1 => "1 item".to_string(),
                n => format!("{} items", crate::notifications::count(n as u64)),
            },
            fs::format_size(bytes)
        ))
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| PathEditor::new(self.dir.to_string(), window, cx));

        cx.subscribe_in(
            &editor,
            window,
            |this, editor, event, window, cx| match event {
                PathEditorEvent::Committed(text) => {
                    match this.resolve_typed_path(text) {
                        Some(dir) => {
                            this.path_editor = None;
                            window.focus(&this.focus_handle, cx);
                            this.navigate_to(Location::Disk(dir), cx);
                        }
                        None => {
                            // Not a directory: flag the field, keep editing.
                            editor.update(cx, |editor, cx| {
                                editor.error = true;
                                cx.notify();
                            });
                        }
                    }
                }
                PathEditorEvent::Edited => {}
                PathEditorEvent::Cancelled => {
                    this.path_editor = None;
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }
                // Abandoned the same way, but without pulling the focus back:
                // it has already gone somewhere the user chose, and another
                // pane is one of the places it can have gone. Except when it
                // has gone nowhere, which is what the window losing focus
                // does; see the same arm on the search field.
                PathEditorEvent::Blurred => {
                    let stranded = editor.focus_handle(cx).is_focused(window);
                    this.path_editor = None;
                    if stranded {
                        window.focus(&this.focus_handle, cx);
                    }
                    cx.notify();
                }
            },
        )
        .detach();

        window.focus(&editor.focus_handle(cx), cx);
        self.path_editor = Some((BarMode::Path, editor));
        cx.notify();
    }

    /// `~` expands to $HOME; a relative path resolves against the current dir.
    fn resolve_typed_path(&self, text: &str) -> Option<PathBuf> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let expanded = if let Some(rest) = text.strip_prefix("~") {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            home.join(rest.trim_start_matches('/'))
        } else {
            PathBuf::from(text)
        };
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            // Relative to a real directory only. Inside an archive there is
            // nothing to resolve against, so only a full path gets you out.
            self.dir.disk()?.join(expanded)
        };
        absolute.is_dir().then_some(absolute)
    }

    fn go_home(&mut self, _: &GoHome, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            self.navigate_to(Location::Disk(home), cx);
        }
    }

    fn go_up(&mut self, _: &GoUp, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.dir.parent() {
            self.navigate_to(parent, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.cursor_ix else { return };
        self.activate(ix, cx);
    }

    /// Open what a row holds: enter a directory, hand a file to the desktop.
    ///
    /// Shared by enter and by double-click, which is the point. They were two
    /// pieces of code doing nearly the same thing, and the difference was that
    /// double-click tested `is_dir` and did nothing at all when the answer was
    /// no, so a file opened from the keyboard and not from the mouse.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        // Inside an archive already: a folder is somewhere to go, a file is
        // not. `xdg-open` needs a path and there is none, so opening one means
        // extracting a temporary copy first, and that needs the user's say-so:
        // see `open_prompt`, and why it asks rather than just doing it.
        if entry.member().is_some() {
            let Location::Archive { archive, .. } = &self.dir else {
                return;
            };
            // Where this row sits in the archive, from the archive's own root.
            // Not the pane's directory joined to the name: a row does not have
            // to have come from the directory being shown, and once a search
            // can return one from further down, `dir.join(name)` names a place
            // that does not exist.
            let Some(member) = entry.key().strip_prefix(archive).ok() else {
                return;
            };
            let (archive, member) = (archive.clone(), member.to_path_buf());
            if entry.is_dir {
                self.navigate_to(
                    Location::Archive {
                        archive,
                        inside: member,
                    },
                    cx,
                );
            } else {
                // The member's *own* directory, not the pane's. `extract`
                // strips this off what it writes, so the temp copy lands as a
                // bare file and `extract_and_open` opens the file rather than
                // the first directory on the way down to it.
                let inside = member.parent().unwrap_or(Path::new("")).to_path_buf();
                cx.emit(PaneEvent::ConfirmExtractAndOpen {
                    archive,
                    inside,
                    member: member.to_string_lossy().into_owned(),
                    name: entry.name.clone(),
                });
            }
            return;
        }

        // Both branches below need a real path: one to list, one to hand to
        // the desktop.
        let Some(path) = entry.on_disk().map(Path::to_path_buf) else {
            return;
        };
        if entry.is_dir {
            self.navigate_to(Location::Disk(path), cx);
            return;
        }
        // An archive is a folder as far as this is concerned. Only `.zip`, and
        // deliberately not the many document formats that are zip files
        // underneath: see `archive::is_archive`.
        if crate::archive::is_archive(&path) {
            self.navigate_to(Location::in_archive(path), cx);
            return;
        }
        if let Err(err) = crate::opener::open(&path) {
            // On the strip rather than only on stderr: a double-click that
            // silently does nothing reads as the click not registering, and
            // there is no other sign that anything was attempted.
            let name = entry.name.clone();
            cx.emit(PaneEvent::Notice {
                message: format!("Could not open {name}: {err}"),
                problem: true,
            });
        }
    }

    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Whatever the grid wanted from the preview cache, it does not want
        // while the list is drawn.
        if self.view.layout == fs::Layout::List {
            crate::preview::forget(cx.entity_id(), cx);
        }
        if let Some(error) = &self.error {
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(cx.theme().status().error)
                .child(error.clone())
                .into_any_element();
        }

        if self.entries.is_empty() && self.reading_bytes.is_some() {
            // An archive whose first rows have not arrived. Not "Empty": a
            // tarball takes as long to list as it takes to decompress, and the
            // largest here is a minute, which is a long time to be told there
            // is nothing in it.
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(cx.theme().colors().text_muted)
                .child("Reading…")
                .into_any_element();
        }

        if self.entries.is_empty() {
            // Also the state while the first background read is in flight.
            return div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(cx.theme().colors().text)
                .child("Empty")
                .into_any_element();
        }

        if self.view.layout == fs::Layout::Grid {
            return self.render_grid(window, cx);
        }

        let list = uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                // One reading for the whole batch. Called inside `render_entry`
                // it was a clock call per row, which is exactly what the
                // Modified column's own comment says was avoided: two files
                // written in the same second must not disagree about how long
                // ago that was, and across a 60-second boundary they did.
                let now = SystemTime::now();
                range.map(|ix| this.render_entry(ix, now, cx)).collect()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_1();

        // Over the list, not beside it: a bar that took width would reflow
        // every row the moment a directory grew past one screen. The wrapper is
        // a flex column, or the list's own `flex_1` has nothing to fill.
        div()
            .flex_1()
            .flex()
            .flex_col()
            .relative()
            .child(list)
            .child(crate::scrollbar::scrollbar(
                crate::scrollbar::ScrollSource::Rows {
                    handle: self.scroll.clone(),
                    rows: self.entries.len(),
                    row_height: ROW_HEIGHT,
                },
                cx.entity(),
                |this: &mut Self| &mut this.scrollbar,
            ))
            .into_any_element()
    }
}

impl Drop for DirPane {
    /// Stop whatever is being read for this pane.
    ///
    /// Dropping the task stops the answer being wanted; it does not stop a
    /// thread part-way through decompressing a gigabyte. Closing a pane while
    /// one is running would otherwise leave it running to the end.
    fn drop(&mut self) {
        if let Some(reading) = &self.reading {
            reading.stop();
        }
    }
}

impl Focusable for DirPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PaneEvent> for DirPane {}

impl Render for DirPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Here rather than at each site that moves the selection: this runs
        // after every one of them, and cannot be forgotten by the next one.
        self.sync_footer(cx);

        // `None` inside an archive, and the pane then takes no drops at all:
        // there is nowhere in it to put a file.
        let here = self.dir.disk().map(Path::to_path_buf);
        let drop_border = cx.theme().colors().drop_target_background;
        let refused = cx.theme().status().error_border;

        div()
            .track_focus(&self.focus_handle)
            .key_context("DirPane")
            // Outside the `when_some` below deliberately. That one skips an
            // archive pane entirely, because there is nowhere in it to put a
            // file — which is exactly the case the cursor has to speak up
            // about, so this cannot be attached only where drops are taken.
            .map(|pane| cursor_follows_drops(pane, here.clone()))
            // The pane body is the fallback target: anything not dropped on a
            // folder row lands in the directory being shown. A folder row that
            // accepts consumes the drop first, so the two never both fire.
            .when_some(here, |pane, here| {
                pane.can_drop(|_, _, _| true)
                    // A border rather than a fill: the whole pane going solid would
                    // hide the listing you are aiming at.
                    .drag_over::<DraggedPaths>({
                        let here = here.clone();
                        move |style, dragged, _, _| {
                            style.border_color(tint(
                                drop_allowed(dragged, &here),
                                drop_border,
                                refused,
                            ))
                        }
                    })
                    .drag_over::<ExternalPaths>({
                        let here = here.clone();
                        move |style, dragged, _, _| {
                            style.border_color(tint(
                                drop_allowed(dragged, &here),
                                drop_border,
                                refused,
                            ))
                        }
                    })
                    .drag_over::<DraggedMembers>(move |style, _, _, _| {
                        style.border_color(drop_border)
                    })
                    .on_drop(cx.listener({
                        let here = here.clone();
                        move |this, dragged: &DraggedPaths, window, cx| {
                            let sources = dragged.paths();
                            let source_dir = dragged.source_dir.clone();
                            this.accept_drop(sources, Some(&source_dir), here.clone(), window, cx);
                        }
                    }))
                    .on_drop(cx.listener({
                        let here = here.clone();
                        move |this, paths: &ExternalPaths, window, cx| {
                            this.accept_drop(
                                paths.paths().to_vec(),
                                None,
                                here.clone(),
                                window,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let here = here.clone();
                        move |this, dragged: &DraggedMembers, _window, cx| {
                            this.accept_extract_drop(
                                dragged.archive.clone(),
                                dragged.inside.clone(),
                                dragged.roots(),
                                here.clone(),
                                cx,
                            );
                        }
                    }))
            })
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::nav_back))
            .on_action(cx.listener(Self::nav_forward))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::edit_path))
            .on_action(cx.listener(Self::start_search))
            .on_action(cx.listener(Self::rename_selected))
            .on_action(cx.listener(Self::toggle_hidden))
            .on_action(cx.listener(Self::toggle_folders_first))
            .on_action(
                cx.listener(|this, _: &ListView, _, cx| this.set_layout(fs::Layout::List, cx)),
            )
            .on_action(
                cx.listener(|this, _: &GridView, _, cx| this.set_layout(fs::Layout::Grid, cx)),
            )
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(true), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(false), cx)))
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.zoom(None, cx)))
            .on_action(cx.listener(Self::refresh_action))
            .on_action(cx.listener(Self::reverse_sort))
            .on_action(cx.listener(Self::sort_by_name))
            .on_action(cx.listener(Self::sort_by_size))
            .on_action(cx.listener(Self::sort_by_kind))
            .on_action(cx.listener(Self::sort_by_modified))
            .on_action(cx.listener(Self::sort_by_permissions))
            .on_action(cx.listener(Self::sort_by_owner))
            .on_action(cx.listener(Self::sort_by_group))
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    // Keys bubble up from descendants, so an open editor or menu
                    // would otherwise type into the listing behind it.
                    if this.has_inline_editor() || this.context_menu.is_some() {
                        return;
                    }
                    // Type-ahead find: unmodified printable keys only. Bound keys
                    // (chords, function keys) carry modifiers or no key_char and
                    // fall through untouched.
                    let mods = &event.keystroke.modifiers;
                    if mods.control || mods.alt || mods.platform || mods.function {
                        return;
                    }
                    if let Some(key_char) = event.keystroke.key_char.as_deref()
                        && this.type_ahead_key(key_char, cx)
                    {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(Motion::Up, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(Motion::Down, cx)))
            .on_action(
                cx.listener(|this, _: &MovePageUp, _, cx| this.move_cursor(Motion::PageUp, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MovePageDown, _, cx| this.move_cursor(Motion::PageDown, cx)),
            )
            .on_action(cx.listener(|this, _: &MoveToTop, _, cx| this.move_cursor(Motion::Top, cx)))
            .on_action(
                cx.listener(|this, _: &MoveToBottom, _, cx| this.move_cursor(Motion::Bottom, cx)),
            )
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| this.extend_cursor(Motion::Up, cx)))
            .on_action(
                cx.listener(|this, _: &ExtendDown, _, cx| this.extend_cursor(Motion::Down, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendPageUp, _, cx| this.extend_cursor(Motion::PageUp, cx)),
            )
            .on_action(cx.listener(|this, _: &ExtendPageDown, _, cx| {
                this.extend_cursor(Motion::PageDown, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ExtendToTop, _, cx| this.extend_cursor(Motion::Top, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendToBottom, _, cx| {
                    this.extend_cursor(Motion::Bottom, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &CursorUp, _, cx| this.focus_cursor(Motion::Up, cx)))
            .on_action(
                cx.listener(|this, _: &CursorDown, _, cx| this.focus_cursor(Motion::Down, cx)),
            )
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.move_cursor(Motion::Left, cx)))
            .on_action(
                cx.listener(|this, _: &MoveRight, _, cx| this.move_cursor(Motion::Right, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendLeft, _, cx| this.extend_cursor(Motion::Left, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendRight, _, cx| this.extend_cursor(Motion::Right, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CursorLeft, _, cx| this.focus_cursor(Motion::Left, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CursorRight, _, cx| this.focus_cursor(Motion::Right, cx)),
            )
            .on_action(cx.listener(Self::toggle_selection))
            .on_action(cx.listener(Self::focus_results))
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.nav_back(&NavBack, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(gpui::NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.nav_forward(&NavForward, window, cx);
                }),
            )
            // Rows stop propagation, so this only sees empty-area right-clicks.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.selected.clear();
                    this.anchor_ix = None;
                    this.cursor_ix = None;
                    this.open_context_menu(event.position, false, window, cx);
                }),
            )
            // `flex_none` matters: PaneAxisElement sizes panes explicitly and taffy
            // must not fight it.
            .size_full()
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(cx.theme().colors().background)
            .text_color(cx.theme().colors().text)
            .child(self.render_toolbar(cx))
            // The column header belongs to the list. Sorting in the grid is
            // in the view menu.
            .when(self.view.layout == fs::Layout::List, |el| {
                el.child(self.render_header(cx))
            })
            .child(self.render_body(window, cx))
            .child(self.render_footer(cx))
            .when_some(self.context_menu.clone(), |el, (position, menu)| {
                el.child(
                    deferred(
                        anchored()
                            .position(position)
                            .snap_to_window_with_margin(px(8.))
                            .child(menu),
                    )
                    .with_priority(1),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layout naming exactly these columns, in this order.
    #[cfg(test)]
    fn layout(names: &[&str]) -> ColumnLayout {
        ColumnLayout::from_names(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn zoom_steps_through_the_card_widths() {
        assert_eq!(zoom_step(DEFAULT_CARD_WIDTH, true), 280);
        assert_eq!(zoom_step(DEFAULT_CARD_WIDTH, false), 180);
        // At either end it stays.
        assert_eq!(zoom_step(360, true), 360);
        assert_eq!(zoom_step(140, false), 140);
        // From a width between steps, the next step that way.
        assert_eq!(zoom_step(200, true), 220);
        assert_eq!(zoom_step(200, false), 180);
        assert_eq!(clamp_card_width(9000), 360);
        assert_eq!(clamp_card_width(1), 140);
    }

    #[test]
    fn the_set_shown_by_default_is_the_one_that_always_was() {
        // The upgrade case. Anyone who has been using hoja has three columns,
        // in this order, and has never been asked about the other three.
        let shown: Vec<Column> = ColumnLayout::default().iter().collect();
        assert_eq!(shown, [Column::Size, Column::Kind, Column::Modified]);
    }

    #[test]
    fn a_column_turned_on_appears_where_the_table_puts_it() {
        // Not at the end, and not where it was named: the slots decide, and
        // showing a column does not move anything.
        let mut columns = ColumnLayout::default();
        columns.toggle(Column::Permissions);
        columns.toggle(Column::Kind);
        assert_eq!(
            columns.iter().collect::<Vec<_>>(),
            [Column::Size, Column::Modified, Column::Permissions]
        );
    }

    #[test]
    fn hiding_every_column_is_allowed() {
        // The Name column then has the whole pane, which is a view somebody
        // may want and is not a broken one.
        let mut columns = ColumnLayout::default();
        for column in Column::ALL {
            if columns.get(column) {
                columns.toggle(column);
            }
        }
        assert_eq!(columns.iter().count(), 0);
    }

    #[test]
    fn a_file_naming_one_column_leaves_the_rest_at_their_defaults() {
        // What makes `{"permissions": true}` a usable line to write by hand:
        // it turns one column on rather than turning the other five off.
        let map = std::collections::HashMap::from([("permissions".to_string(), true)]);
        assert_eq!(
            ColumnLayout::from_flags(ColumnLayout::default(), &map)
                .iter()
                .collect::<Vec<_>>(),
            [
                Column::Size,
                Column::Kind,
                Column::Modified,
                Column::Permissions
            ]
        );
    }

    #[test]
    fn a_column_that_no_longer_exists_cannot_resurrect_itself() {
        let map = std::collections::HashMap::from([("inode".to_string(), true)]);
        assert_eq!(
            ColumnLayout::from_flags(ColumnLayout::default(), &map),
            ColumnLayout::default()
        );
        assert_eq!(layout(&["inode", "size"]).names(), ["size"]);
    }

    #[test]
    fn a_list_is_the_columns_it_names_in_the_order_it_names_them() {
        let columns = layout(&["permissions", "size", "modified"]);
        assert_eq!(
            columns.iter().collect::<Vec<_>>(),
            [Column::Permissions, Column::Size, Column::Modified]
        );
        assert!(!columns.get(Column::Kind), "and nothing it leaves out");
    }

    #[test]
    fn a_list_survives_being_written_and_read_back() {
        let columns = layout(&["group", "modified", "size"]);
        assert_eq!(ColumnLayout::from_names(&columns.names()), columns);
    }

    #[test]
    fn a_name_given_twice_counts_once() {
        // A hand-written file is the only thing that can say this, and the
        // alternative to ignoring it is drawing a column twice.
        assert_eq!(layout(&["size", "size", "kind"]).names(), ["size", "kind"]);
    }

    #[test]
    fn an_empty_list_draws_nothing_and_is_still_a_permutation() {
        let columns = layout(&[]);
        assert_eq!(columns.iter().count(), 0);
        // The invariant the whole type rests on, checked where it is most
        // likely to be broken: a list that named none of them.
        let mut slots = columns.order.to_vec();
        slots.sort_by_key(|c| *c as usize);
        assert_eq!(slots, Column::ALL.to_vec());
    }

    #[test]
    fn moving_a_column_leaves_a_permutation_behind() {
        let mut columns = ColumnLayout::default();
        for slot in 0..3 {
            columns = columns.moved(Column::Modified, slot);
            let mut slots = columns.order.to_vec();
            slots.sort_by_key(|c| *c as usize);
            assert_eq!(slots, Column::ALL.to_vec(), "moved to slot {slot}");
        }
    }

    #[test]
    fn moving_a_column_to_its_own_slot_changes_nothing() {
        let columns = ColumnLayout::default();
        assert_eq!(columns.moved(Column::Kind, 1), columns);
    }

    #[test]
    fn a_column_moves_past_the_hidden_ones_without_counting_them() {
        // Slots count what is drawn. Kind is hidden between Size and Modified
        // here, so moving Permissions to slot 0 puts it before Size, and Kind
        // stays where it was rather than being dragged along.
        let columns = layout(&["size", "modified", "permissions"]);
        let moved = columns.moved(Column::Permissions, 0);
        assert_eq!(moved.names(), ["permissions", "size", "modified"]);
        assert!(!moved.get(Column::Kind));
    }

    #[test]
    fn a_slot_past_the_end_lands_at_the_end() {
        // The drag can ask for one: the arithmetic works in pointer distance,
        // and the pointer can leave the header entirely.
        let columns = ColumnLayout::default();
        assert_eq!(
            columns.moved(Column::Size, 99).names(),
            ["kind", "modified", "size"]
        );
    }

    #[test]
    fn a_hidden_column_cannot_be_moved() {
        // It has no slot to move to. Silently, because the only caller is a
        // drag on a header that is not being drawn.
        let columns = ColumnLayout::default();
        assert_eq!(columns.moved(Column::Owner, 0), columns);
    }

    /// Three slots 100, 60 and 140 apart: pitches rather than widths, so a
    /// column is 5px narrower than each figure here. Different enough from each
    /// other that an implementation treating them as equal fails.
    #[cfg(test)]
    const W: [f32; 3] = [100., 60., 140.];

    #[test]
    fn a_column_stays_put_until_it_passes_half_of_its_neighbour() {
        // The half is what makes the swap happen where the eye expects it,
        // as the two centres cross, rather than when the dragged column has
        // cleared its neighbour entirely.
        assert_eq!(target_slot(&W, 0, 29.), 0);
        assert_eq!(target_slot(&W, 0, 31.), 1);
        // And it is the *neighbour's* half, not its own: moving right from
        // slot 1 crosses a 140-wide column, not another 60-wide one.
        assert_eq!(target_slot(&W, 1, 69.), 1);
        assert_eq!(target_slot(&W, 1, 71.), 2);
    }

    #[test]
    fn a_long_drag_passes_more_than_one() {
        // 60 to clear the first, then half of 140 to take the second.
        assert_eq!(target_slot(&W, 0, 129.), 1);
        assert_eq!(target_slot(&W, 0, 131.), 2);
    }

    #[test]
    fn it_works_the_same_way_backwards() {
        assert_eq!(target_slot(&W, 2, -29.), 2);
        assert_eq!(target_slot(&W, 2, -31.), 1);
        // 60 to clear the middle column, then half of the 100-wide one.
        assert_eq!(target_slot(&W, 2, -109.), 1);
        assert_eq!(target_slot(&W, 2, -111.), 0);
    }

    #[test]
    fn a_drag_that_leaves_the_header_stops_at_the_end() {
        // The pointer can go anywhere; the slots cannot.
        assert_eq!(target_slot(&W, 0, 100_000.), 2);
        assert_eq!(target_slot(&W, 2, -100_000.), 0);
        assert_eq!(target_slot(&W, 1, 0.), 1);
    }

    #[test]
    fn asking_twice_gives_the_same_answer() {
        // The property the whole drag rests on. Every pointer move recomputes
        // this from the layout the drag started with, so it has to depend on
        // nothing but its arguments; an implementation that walked the live
        // order instead would drift a slot per frame.
        let columns = ColumnLayout::default();
        let slot = target_slot(&W, 0, 131.);
        let once = columns.moved(Column::Size, slot);
        assert_eq!(once, once.moved(Column::Size, target_slot(&W, 0, 131.)));
        assert_eq!(once.names(), ["kind", "modified", "size"]);
    }

    #[test]
    fn a_column_is_wide_enough_for_what_it_prints() {
        // The numeric face is wider than the proportional one it replaced, and
        // the failure is silent: a timestamp becomes "2026-08-02 13…" and looks
        // like a design choice rather than a column two pixels short.
        for column in Column::ALL {
            let widest = column.widest();
            if widest.is_empty() {
                continue;
            }
            let advance = if column.is_numeric() {
                ROW_CHAR_W
            } else {
                PROPORTIONAL_CHAR_W
            };
            let needed = widest.chars().count() as f32 * advance + CELL_PADDING;
            // Against the minimum, not the default. A default only reaches
            // someone installing for the first time; everyone else restores a
            // width from disk, and the minimum is the floor that applies to it.
            assert!(
                needed <= f32::from(column.min_width()),
                "{:?} needs {needed}px for {widest:?} but its floor is {:?}",
                column,
                column.min_width()
            );
            assert!(
                column.min_width() <= column.default_width(),
                "{column:?} starts narrower than it is allowed to be"
            );
        }
    }

    #[test]
    fn only_columns_of_words_may_be_dragged_narrower_than_their_content() {
        // Kind, Owner and Group: their content has no fixed length, so a floor
        // sized to it would be a guess. The rest hold figures or letters of a
        // known widest form and their floors are those.
        for column in [Column::Kind, Column::Owner, Column::Group] {
            assert_eq!(column.min_width(), px(COL_MIN_WIDTH), "{column:?}");
        }
        for column in Column::ALL {
            assert_eq!(
                column.min_width() == px(COL_MIN_WIDTH),
                column.widest().is_empty(),
                "{column:?}: the bare minimum is for columns with no widest form, \
                 and every column that has one needs a floor sized to it"
            );
        }
        for column in Column::ALL {
            assert!(f32::from(column.min_width()) <= f32::from(column.default_width()));
        }
    }
}
