use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use file_icons::FileIcons;
use gpui::AnimationExt as _;
use gpui::{
    App, ClickEvent, Context, DismissEvent, DragMoveEvent, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription,
    Task, UniformListScrollHandle, Window, actions, anchored, deferred, div, prelude::*, px,
    uniform_list,
};
use hoja_transfer::Operation;
use theme::ActiveTheme;

use crate::file_menu::{FileMenu, MenuItem};
use crate::fs::{self, DirEntry, Sort, SortDir, SortKey, ViewSettings};
use crate::git::GitStatus;
use crate::history::History;
use crate::icon::Icon;
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
        ToggleSelection,
        NavBack,
        NavForward,
        GoHome,
        EditPath,
        StartSearch,
        RenameSelected,
        ToggleHiddenFiles,
        ToggleFoldersFirst,
        Refresh,
        ReverseSort,
        SortByName,
        SortBySize,
        SortByKind,
        SortByModified,
    ]
);

/// Poll step for the directory watcher. Two of these bound how long a change
/// made elsewhere stays invisible, and also how often a busy directory re-lists.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

const ROW_HEIGHT: f32 = 22.;
const HEADER_HEIGHT: f32 = 24.;
/// Grab area between two column headers.
const COL_HANDLE_WIDTH: f32 = 5.;
const COL_MIN_WIDTH: f32 = 56.;
/// The bar that stands in for a folder size still being counted, and the breath
/// it takes. Narrower than the figure it precedes, and faint at both ends of the
/// cycle: it is the answer to "is anything happening", not a thing to read.
const COUNTING_BAR_WIDTH: f32 = 38.;
const COUNTING_BAR_HEIGHT: f32 = 4.;
const COUNTING_PERIOD: Duration = Duration::from_millis(1600);
const COUNTING_ALPHA_LOW: f32 = 0.10;
const COUNTING_ALPHA_HIGH: f32 = 0.32;
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
const COL_MAX_WIDTH: f32 = 420.;
/// The Name column flexes to fill, but never below this.
const NAME_MIN_WIDTH: f32 = 100.;

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
    /// `problem` because these are not all failures. "Extract it first to open
    /// it" is an answer to a question, and colouring it like a broken transfer
    /// says the wrong thing about it.
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
}

/// Whether a drag may land on `target`, for either payload type.
///
/// `can_drop` runs before the drop and also gates the highlight, so refusing
/// here means an illegal target never lights up. Note that a refusal *consumes*
/// the drop rather than passing it to the element behind, which is what we
/// want, but is not a fall-through.
fn drop_allowed(dragged: &dyn std::any::Any, target: &Path) -> bool {
    if let Some(dragged) = dragged.downcast_ref::<DraggedPaths>() {
        return fs::is_valid_drop(&dragged.paths(), target);
    }
    if let Some(paths) = dragged.downcast_ref::<ExternalPaths>() {
        return fs::is_valid_drop(paths.paths(), target);
    }
    false
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
    /// Dropping this stops the walk.
    handle: Option<crate::search::Search>,
    _poll: Task<()>,
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
fn quieted(mut color: gpui::Hsla, active: bool) -> gpui::Hsla {
    if !active {
        color.a *= INACTIVE_CONTENT_ALPHA;
    }
    color
}

/// Rows being dragged, and where they came from so a drop back onto their own
/// directory can be refused.
///
/// What the drag carries is settled *once*, when the drag starts, and carried
/// from there: neither gathered per frame nor looked up again at the end. Both
/// of the other timings are wrong, and each was a bug:
///
/// - Gathering it eagerly, at render time, cost 3.9ms a frame with 100k rows
///   selected, for a payload nearly every frame throws away.
/// - Looking it up at drop time read the source pane's *live* selection, which
///   is not necessarily the one being dragged: a paste landing mid-drag
///   re-selects what it wrote, so the drop moved files nobody dragged. Worse,
///   the drop handler is a listener on the pane being dropped into, which
///   leases it, so reading the source pane there panicked outright whenever
///   the two were the same pane, which is to say on every same-pane drop.
#[derive(Clone)]
pub struct DraggedPaths {
    /// Filled by `resolve`, below.
    resolved: std::cell::OnceCell<Vec<PathBuf>>,
    /// Only read before the drag starts; nothing touches it afterwards.
    pane: Entity<DirPane>,
    /// The row the drag started on, used when it was not part of the selection.
    anchor: PathBuf,
    whole_selection: bool,
    pub source_dir: PathBuf,
}

impl DraggedPaths {
    /// Settle what this drag carries.
    ///
    /// Called from the drag-preview builder, which gpui runs exactly when a
    /// drag begins, on the very payload it then stores for the drag's lifetime
    /// (`div.rs:2844` renders from `listener.value` and `div.rs:2860` moves
    /// that same `Arc` into `active_drag`). So filling the cell there is what
    /// makes every later reader agree, and it happens while dispatching a mouse
    /// move: no entity is leased.
    fn resolve(&self, cx: &App) {
        self.resolved.get_or_init(|| {
            // The anchor is the fallback, and it is always a real path: the
            // drag is not offered at all for a row without one. So a selection
            // that spans an archive and the disk, which has no set of real
            // paths to carry, drags the row that was grabbed instead.
            self.whole_selection
                .then(|| self.pane.read(cx).selected_on_disk())
                .flatten()
                .unwrap_or_else(|| vec![self.anchor.clone()])
        });
    }

    /// Takes no `cx` on purpose: after the drag has started there is nothing
    /// left to read. The fallback cannot be reached through gpui's own drag
    /// path, and dragging the one row you grabbed is the safe reading of it.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.resolved
            .get()
            .cloned()
            .unwrap_or_else(|| vec![self.anchor.clone()])
    }
}

/// What the cursor carries during a drag. The platform draws its own icons for
/// a drag that leaves the window, so this is only ever seen inside pane.
struct DragPreview {
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border_selected)
            .bg(colors.elevated_surface_background)
            .text_sm()
            .text_color(colors.text)
            .child(self.label.clone())
    }
}

/// A fixed-width column after the Name column.
///
/// Name is not a variant: it flexes to fill whatever these leave, holds the icon
/// and the rename editor, and has no divider of its own, so it is built directly
/// rather than driven from this table.
///
/// Everything about a column lives here, its order, label, sort key, starting
/// width, and the text it shows, so a new column is one variant, one entry in
/// `ALL`, and one arm in each method. The header, the rows, and the resize
/// handles all follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Size,
    Kind,
    Modified,
}

impl Column {
    /// Display order. The length is part of the type, so adding a variant here
    /// makes the compiler point at `ColumnWidths` rather than silently leaving
    /// the new column without a width.
    const ALL: [Column; 3] = [Column::Size, Column::Kind, Column::Modified];

    /// The key this column's width is stored under. Deliberately not the
    /// display label: renaming what the header says must not discard the width
    /// everyone has already dragged.
    fn key(self) -> &'static str {
        match self {
            Column::Size => "size",
            Column::Kind => "kind",
            Column::Modified => "modified",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Column::Size => "Size",
            Column::Kind => "Kind",
            Column::Modified => "Modified",
        }
    }

    fn sort_key(self) -> SortKey {
        match self {
            Column::Size => SortKey::Size,
            Column::Kind => SortKey::Kind,
            Column::Modified => SortKey::Modified,
        }
    }

    /// Whether the column's content is held to its right edge.
    ///
    /// Size only, because it is the only column where the digits carry the
    /// meaning: right-aligned, magnitude reads down the column rather than one
    /// row at a time. It aligns the unit, not the decimal point, so a "400 B"
    /// among kilobytes still sits one character right of its neighbours; true
    /// decimal alignment would mean splitting the cell into a right-aligned
    /// number and a left-aligned unit, as the transfer strip does.
    ///
    /// Modified gains nothing from it, since a timestamp is a fixed sixteen
    /// characters and its two edges are the same edge. Kind is words.
    fn aligns_right(self) -> bool {
        matches!(self, Column::Size)
    }

    /// Whether the column prints figures, and so is set in the numeric face.
    ///
    /// Size alone. Kind and Modified are both words now: "Folder", "BIN",
    /// "3 hours ago". Words in a monospaced face are just words set badly.
    fn is_numeric(self) -> bool {
        matches!(self, Column::Size)
    }

    /// Wide enough for the widest thing the column prints, at the row's text
    /// size in the numeric face. Monospace is wider than the proportional face
    /// it replaced, so the old 90 and 140 truncated a full timestamp and a
    /// four-digit size the day the font changed. `widest` pins both.
    fn default_width(self) -> Pixels {
        match self {
            Column::Size => px(100.),
            Column::Kind => px(90.),
            Column::Modified => px(132.),
        }
    }

    /// How narrow this column may be dragged, and the floor a width restored
    /// from disk is lifted to.
    ///
    /// Per column, because a default is not reachable once anyone has used the
    /// program: `remember_view` writes all three widths on any view change, so
    /// every existing install carries the previous release's numbers, and
    /// raising a default only helps someone installing for the first time. The
    /// Size column widened for the monospaced face and everybody kept 90, which
    /// is two pixels short of "1023.0 MB".
    ///
    /// Kind keeps the bare minimum: its content is words of no fixed length and
    /// eliding them is what the column is for.
    fn min_width(self) -> Pixels {
        match self {
            Column::Kind => px(COL_MIN_WIDTH),
            Column::Size => px(94.),
            Column::Modified => px(124.),
        }
    }

    /// The longest string this column ever holds, for the width test.
    #[cfg(test)]
    fn widest(self) -> &'static str {
        match self {
            // `format_size` always prints one decimal and promotes past 1023,
            // so four digits, a point, a decimal and a two-letter unit is the
            // most it can come to.
            Column::Size => "1023.9 MB",
            // Not pinned: "Rust source" and the like come from the file's kind
            // and are already elided when they do not fit.
            Column::Kind => "",
            // The longest `format_time_ago` prints, pinned by its own test.
            Column::Modified => "59 minutes ago",
        }
    }

    /// The cell text for one entry. Empty where the entry has no such value:
    /// unreadable metadata has no time, and a folder has no size until the walk
    /// behind the footer has finished counting it, which is what `folder_bytes`
    /// supplies. Blank until then rather than a figure that climbs: one number
    /// moving at the bottom of the pane reads as progress, a listing full of
    /// them reads as churn.
    fn value(self, entry: &DirEntry, folder_bytes: Option<u64>, now: SystemTime) -> String {
        match self {
            Column::Size => entry
                .size
                .or(folder_bytes)
                .map(fs::format_size)
                .unwrap_or_default(),
            Column::Kind => entry.kind(),
            // Read once for the batch of rows being built and passed down,
            // rather than a clock call per row: the answer must be the same for
            // every row in a listing, or two files written in the same second
            // can disagree about how long ago that was.
            Column::Modified => entry
                .modified
                .map(|at| fs::format_time_ago(at, now))
                .unwrap_or_default(),
        }
    }
}

/// Per-column widths, positional so the set can grow without new fields.
///
/// Indexed by discriminant, not by position in `ALL`, so `ALL` must hold every
/// variant exactly once: otherwise a column indexes a width that belongs to
/// another, or runs off the end.
#[derive(Clone, Copy, Debug)]
struct ColumnWidths([Pixels; Column::ALL.len()]);

impl Default for ColumnWidths {
    fn default() -> Self {
        let mut widths = [px(0.); Column::ALL.len()];
        for column in Column::ALL {
            widths[column as usize] = column.default_width();
        }
        Self(widths)
    }
}

impl ColumnWidths {
    fn get(&self, column: Column) -> Pixels {
        self.0[column as usize]
    }

    fn set(&mut self, column: Column, width: Pixels) {
        self.0[column as usize] = width.clamp(column.min_width(), px(COL_MAX_WIDTH));
    }
}

/// A keyboard movement of the lead row. Each has a plain form that takes the
/// selection with it and a shift form that extends the selection instead.
#[derive(Clone, Copy)]
enum Motion {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// What `on_drag_move` dispatches on, and who started it.
///
/// The type alone is not enough. gpui routes a drag-move to every handler
/// registered for the payload's *type*, so a resize in one pane is delivered to
/// the identically-typed handlers in every other pane as well. Carrying the
/// pane and the column means each handler can tell whether the drag is its own,
/// which the pane's own `resize` anchor cannot: that is set on mouse down and
/// never cleared, so a pane that was ever resized keeps an anchor from a drag
/// that ended long ago. A second pane then answered someone else's drag with a
/// start position hundreds of pixels away and drove its column straight into
/// `COL_MIN_WIDTH`.
#[derive(Clone, Copy)]
struct ColumnResize {
    pane: gpui::EntityId,
    column: Column,
}

/// Where a column resize started, captured once when the drag begins.
#[derive(Clone, Copy)]
struct ColumnDrag {
    column: Column,
    start_x: Pixels,
    start_width: Pixels,
}

/// One fixed-width cell of a listing row, resolved before the element is built.
///
/// A struct rather than a tuple because the call site was passing five
/// positional arguments, two of them bare bools, and `cell(w, t, n, true, false)`
/// says nothing about which of those is the alignment.
struct Cell {
    width: Pixels,
    text: String,
    /// The monospaced family, for a column that prints figures.
    numeric: Option<gpui::SharedString>,
    /// Show the breathing bar rather than `text`, which is empty anyway while
    /// the size behind it is still being counted.
    counting: bool,
    right: bool,
}

/// Drags need a preview view; column resizing shows nothing.
struct EmptyDrag;

impl Render for EmptyDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
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
    /// Whether the scrollbar is mid-drag. The bar is rebuilt every frame; a
    /// drag is not.
    scrollbar: crate::scrollbar::ScrollbarState,
    /// Git status by entry name for the loaded directory. Empty outside a
    /// repository, and empty until the background query lands.
    git: crate::git::GitStatuses,
    git_task: Option<Task<()>>,
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
        let subscriptions = vec![cx.on_focus_in(&focus_handle, window, |_, _, cx| {
            cx.emit(PaneEvent::Focus);
        })];

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
            scrollbar: crate::scrollbar::ScrollbarState::default(),
            git: crate::git::GitStatuses::new(),
            git_task: None,
            watch: None,
            view,
            loaded_dir: None,
            error: None,
            load_task: None,
            reading_bytes: None,
            reading: None,
            sort_task: None,
            _clock: Self::spawn_clock(cx),
            context_menu: None,
            path_editor: None,
            search: None,
            renaming: None,
            pending_select: Vec::new(),
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
    pub fn probe(&self, now: std::time::SystemTime) -> crate::probe::PaneProbe {
        let column = |which: Column| {
            self.entries
                .iter()
                .take(crate::probe::MAX_ROWS)
                .enumerate()
                .map(|(ix, entry)| which.value(entry, self.folder_bytes(ix), now))
                .collect()
        };
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
            selected: self.selected.iter().collect(),
            cursor: self.cursor_ix,
            footer: self.footer_line(),
            counting: (0..self.entries.len().min(crate::probe::MAX_ROWS))
                .filter(|ix| self.counting_size(*ix))
                .collect(),
            searching: self.searching(),
            reading: self.reading_bytes.is_some(),
            error: self.error.clone(),
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

    /// Replace the view wholesale, as a settings edit does. Re-lists, because
    /// hidden files and the sort order both change what the rows are.
    pub fn set_view_settings(&mut self, view: ViewSettings, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.relist(cx);
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

    /// Summon the context menu at `position` (window coords, from the mouse event).
    fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        on_rows: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_focus = self.focus_handle.clone();
        // Item handlers refocus the pane BEFORE dispatching, otherwise the
        // action dispatches at the menu's focus node where no handler exists.
        let dispatch = move |action: Box<dyn gpui::Action>| {
            let pane_focus = pane_focus.clone();
            move |window: &mut Window, cx: &mut App| {
                window.focus(&pane_focus, cx);
                window.dispatch_action(action.boxed_clone(), cx);
            }
        };

        // Nothing here changes an archive: this module only reads them, which
        // is what makes rename, delete and the rest refusals rather than
        // half-built features. Absent rather than present and refusing, because
        // a menu of things that all say no is worse than a shorter menu.
        let writable = self.dir.is_disk();

        let mut items = Vec::new();
        // Where the "Open With" section lands once it resolves.
        let mut open_with_ix = None;
        if on_rows {
            items.push(MenuItem::action("Open", dispatch(Box::new(OpenSelected))));

            // MIME detection reads the head of the file, so it can block for as
            // long as the filesystem wants to take: unacceptable on a network
            // mount. The menu opens without this section and grows it in.
            // Only for a real file: the applications this offers are launched
            // on a path, and there is nothing to hand them for a row that has
            // none.
            let anchor_entry = self.cursor_ix.and_then(|ix| self.entries.get(ix));
            if let Some(path) = anchor_entry
                .filter(|e| !e.is_dir)
                .and_then(|entry| entry.on_disk())
            {
                open_with_ix = Some((items.len(), path.to_path_buf()));
            }
            if writable {
                items.push(MenuItem::Separator);
                items.push(MenuItem::action(
                    "Rename",
                    dispatch(Box::new(RenameSelected)),
                ));
                items.push(MenuItem::action(
                    "Delete",
                    dispatch(Box::new(workspace::Delete)),
                ));
                items.push(MenuItem::Separator);
                items.push(MenuItem::action("Cut", dispatch(Box::new(workspace::Cut))));
                items.push(MenuItem::action(
                    "Copy",
                    dispatch(Box::new(workspace::Copy)),
                ));
            }
        }
        if writable {
            items.push(MenuItem::action(
                "Paste",
                dispatch(Box::new(workspace::Paste)),
            ));
            items.push(MenuItem::Separator);
            items.push(MenuItem::action(
                "New Folder",
                dispatch(Box::new(workspace::NewFolder)),
            ));
        }

        self.show_menu(items, position, window, cx);

        if let Some((ix, path)) = open_with_ix {
            self.resolve_open_with(ix, path, cx);
        }
    }

    /// Resolve registered applications off the UI thread, then splice them into
    /// the open menu. A menu dismissed before this lands drops the result.
    fn resolve_open_with(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
        let apps = cx.background_spawn(async move { crate::opener::apps_for(&path) });
        cx.spawn(async move |this, cx| {
            let apps = apps.await;
            if apps.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                let Some((_, menu)) = this.context_menu.as_ref() else {
                    return;
                };
                let mut items = vec![MenuItem::Separator];
                items.extend(apps.into_iter().take(8).map(|app| {
                    MenuItem::action(format!("Open with {}", app.name), move |_, _| {
                        if let Err(err) = crate::opener::launch(&app) {
                            eprintln!("[hoja] launch failed: {err}");
                        }
                    })
                }));
                menu.update(cx, |menu, cx| menu.insert_items(ix, items, cx));
            })
            .ok();
        })
        .detach();
    }

    /// The pane view menu behind the hamburger button.
    fn open_settings_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane_focus = self.focus_handle.clone();
        let dispatch = move |action: Box<dyn gpui::Action>| {
            let pane_focus = pane_focus.clone();
            move |window: &mut Window, cx: &mut App| {
                window.focus(&pane_focus, cx);
                window.dispatch_action(action.boxed_clone(), cx);
            }
        };

        let items = vec![
            MenuItem::toggle(
                "Show Hidden Files",
                self.view.show_hidden,
                dispatch(Box::new(ToggleHiddenFiles)),
            ),
            MenuItem::toggle(
                "Folders First",
                self.view.folders_first,
                dispatch(Box::new(ToggleFoldersFirst)),
            ),
            MenuItem::Separator,
            MenuItem::toggle(
                "Sort by Name",
                self.view.sort.key == SortKey::Name,
                dispatch(Box::new(SortByName)),
            ),
            MenuItem::toggle(
                "Sort by Size",
                self.view.sort.key == SortKey::Size,
                dispatch(Box::new(SortBySize)),
            ),
            MenuItem::toggle(
                "Sort by Kind",
                self.view.sort.key == SortKey::Kind,
                dispatch(Box::new(SortByKind)),
            ),
            MenuItem::toggle(
                "Sort by Modified",
                self.view.sort.key == SortKey::Modified,
                dispatch(Box::new(SortByModified)),
            ),
            MenuItem::Separator,
            MenuItem::toggle(
                "Reverse Order",
                self.view.sort.dir == SortDir::Descending,
                dispatch(Box::new(ReverseSort)),
            ),
            MenuItem::Separator,
            MenuItem::action("Refresh", dispatch(Box::new(Refresh))),
        ];

        self.show_menu(items, position, window, cx);
    }

    /// True while an inline editor owns the keyboard. New editors must be added
    /// here rather than to each dismiss handler.
    fn has_inline_editor(&self) -> bool {
        self.renaming.is_some() || self.path_editor.is_some()
    }

    /// Shared summoning for the context and settings menus: build, subscribe,
    /// deferred focus, store in the single overlay slot.
    fn show_menu(
        &mut self,
        items: Vec<MenuItem>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| FileMenu::new(items, window, cx));

        cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.context_menu = None;
            // A menu item may have started an inline edit (Rename, path edit);
            // refocusing the pane here would stomp the editor's focus.
            if !this.has_inline_editor() {
                window.focus(&this.focus_handle, cx);
            }
            cx.notify();
        })
        .detach();

        // Deferred draws join the dispatch tree a frame late; focusing the menu
        // needs the double hop or the blur-dismiss fires immediately.
        let menu_focus = menu.focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                window.focus(&menu_focus, cx);
            });
        });

        self.context_menu = Some((position, menu));
        cx.notify();
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
            self.scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
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

    /// Where `motion` lands, or `None` when the listing is empty.
    fn destination(&self, motion: Motion) -> Option<usize> {
        let len = self.entries.len();
        let step = |delta: isize| fs::step_row(len, self.cursor_ix, delta);
        match motion {
            Motion::Up => step(-1),
            Motion::Down => step(1),
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
        self.scroll
            .scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        cx.notify();
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

    /// Turn a completed drop into a transfer request.
    ///
    /// `dest` is the folder that was dropped on, which is a row's own path for a
    /// folder row and the pane's directory for the body.
    fn accept_drop(
        &mut self,
        sources: Vec<PathBuf>,
        source_dir: Option<&Path>,
        dest: PathBuf,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() || !fs::is_valid_drop(&sources, &dest) {
            return;
        }

        let op = match source_dir {
            // An internal drag: the events are real, so the modifiers are live.
            // Within one filesystem a move is a rename and free; across one it
            // is a copy plus a delete, so the convention is to move within and
            // copy across.
            Some(from) => {
                let modifiers = window.modifiers();
                let move_it = !modifiers.control
                    && (modifiers.shift || hoja_transfer::same_filesystem(from, &dest));
                if move_it {
                    Operation::Move
                } else {
                    Operation::Copy
                }
            }
            // From another application. gpui accepts external offers with
            // DndAction::Copy hardcoded, so the source has been told this is a
            // copy: moving would delete data it still believes it owns. The
            // same translation also wipes the modifiers, so there is nothing to
            // read even if we wanted to offer a choice.
            None => Operation::Copy,
        };

        cx.emit(PaneEvent::Transfer { op, sources, dest });
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

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                eprintln!("[hoja] directory watcher unavailable: {err}");
                return;
            }
        };

        use notify::Watcher as _;
        if let Err(err) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            // An unreadable or vanished directory is not worth a message: the
            // listing itself already reports that.
            let _ = err;
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            // Keep the watcher alive for the lifetime of this task.
            let _watcher = watcher;
            loop {
                // Poll rather than blocking on `recv`, so the receiver stays
                // owned by this task instead of moving into a new future each
                // iteration.
                cx.background_executor().timer(WATCH_INTERVAL).await;

                if !crate::config::drain_changes(&rx) {
                    continue;
                }

                // A copy or an extract emits one event per file. Coalesce the
                // burst so a long operation re-lists a few times rather than
                // thousands, which matters when the directory is large.
                cx.background_executor().timer(WATCH_INTERVAL).await;
                crate::config::drain_changes(&rx);

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
        // Clear first: the old directory's statuses must not tint this one.
        self.git = crate::git::GitStatuses::new();
        // `git -C` needs a directory to run in, and a repository cannot be
        // inside an archive anyway.
        let Some(dir) = self.dir.disk().map(Path::to_path_buf) else {
            return;
        };
        self.git_task = Some(cx.spawn(async move |this, cx| {
            let statuses = cx
                .background_spawn(async move { crate::git::statuses(&dir) })
                .await;
            if statuses.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.git = statuses;
                cx.notify();
            });
        }));
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
        let sort = self.view.sort;
        let folders_first = self.view.folders_first;

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
                let index = crate::archive::Index::build(listing);
                let rows = crate::archive::rows_in(&index, &archive, &inside);

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

                    match rows {
                        Some(rows) => {
                            let mut entries = rows.entries;
                            fs::sort_entries(&mut entries, sort, folders_first);
                            this.entries = entries;
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
        // A set, not the Vec: this runs once per entry, so a linear scan makes
        // restoring a large selection quadratic: measured at 11 seconds for
        // 100k rows selected, on the foreground executor.
        let wanted: std::collections::HashSet<PathBuf> = std::mem::take(&mut self.pending_select)
            .into_iter()
            .collect();
        self.selected.clear();
        self.anchor_ix = None;
        self.cursor_ix = None;
        if wanted.is_empty() {
            self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
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
            Some(ix) => self
                .scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest),
            None => self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top),
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
                this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
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
        self.history.push(dir.clone());
        self.load_dir_only(dir, cx);
    }

    /// Change directory without touching history, the back/forward path.
    fn load_dir_only(&mut self, dir: Location, cx: &mut Context<Self>) {
        self.dir = dir;
        self.reload(cx);
        cx.notify();
    }

    fn nav_back(&mut self, _: &NavBack, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.history.back().cloned() {
            self.load_dir_only(dir, cx);
        }
    }

    fn nav_forward(&mut self, _: &NavForward, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.history.forward().cloned() {
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
                PathEditorEvent::Cancelled => {
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
        let editor = cx
            .new(|cx| PathEditor::new(initial, window, cx).with_placeholder("Search this folder"));

        cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
            PathEditorEvent::Edited => {}
            // Enter hands the listing back the focus and *keeps* the search, so
            // the arrow keys work on the results. They cannot work while the
            // field has focus, because every pane binding is masked by
            // `!AddressBar`. Escape from there clears it.
            PathEditorEvent::Committed(_) => {
                this.path_editor = None;
                window.focus(&this.focus_handle, cx);
                cx.notify();
            }
            PathEditorEvent::Cancelled => {
                this.end_search(window, cx);
                cx.notify();
            }
        })
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
                self.entries = previous.listing;
                self.listing_moved += 1;
                self.clear_cursor();
                self.restore_selection();
                self.reload(cx);
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
        // it to walk inside an archive. Filtering the rows already listed still
        // works, and that is a different code path.
        let Some(dir) = self.dir.disk().map(Path::to_path_buf) else {
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
                    if let Some(search) = this.search.as_mut() {
                        search.handle = Some(crate::search::spawn(dir, spawn_query, show_hidden));
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
                    let Some(handle) = this.search.as_ref().and_then(|s| s.handle.as_ref()) else {
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
            handle: None,
            _poll: poll,
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
        if relisted || self.footer.selection != self.selected.revision() {
            self.footer.selection = self.selected.revision();
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
            fs::summarise_selection(&self.entries, &self.selected)
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
    fn end_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((BarMode::Search, _)) = &self.path_editor {
            self.path_editor = None;
        }
        self.set_filter(None, cx);
        window.focus(&self.focus_handle, cx);
    }

    /// A word about the results, since the listing is no longer this directory.
    /// Shown in this pane's own footer: a search belongs to the pane running it,
    /// and in the window-wide strip only the active pane's was ever visible.
    pub fn search_status(&self) -> Option<String> {
        let search = self.search.as_ref()?;
        let found = self.entries.len();
        // No handle yet means the debounce has not elapsed.
        let Some(handle) = search.handle.as_ref() else {
            return Some("searching…".to_string());
        };
        Some(if !handle.is_done() {
            format!("searching… {found}")
        } else if handle.hit_cap() {
            format!("first {found} matches")
        } else {
            match found {
                0 => "no matches".to_string(),
                1 => "1 match".to_string(),
                n => format!("{n} matches"),
            }
        })
    }

    /// The footer line while an archive is being read, which for a tarball can
    /// be the best part of a minute.
    ///
    /// Says the same two things the settled line does, so the numbers do not
    /// jump around when it lands: what is here, and how much of it. The
    /// ellipsis marks them as still climbing, which is the shape the job strip
    /// already uses for a total it does not have yet.
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
        // not. `xdg-open` needs a path and there is none, and extracting to a
        // temporary copy would quietly throw away anything written to it, which
        // is the trap every archive tool has shipped and regretted.
        if entry.member().is_some() {
            if entry.is_dir {
                let name = entry.name.clone();
                self.navigate_to(self.dir.join(name), cx);
            } else {
                cx.emit(PaneEvent::Notice {
                    message: "Extract it first to open it".to_string(),
                    problem: false,
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

    /// A divider sitting at the left edge of `column`.
    ///
    /// The width is computed *absolutely*, from the cursor's offset since the drag
    /// began. The obvious alternative: nudge the width by how far the cursor has
    /// drifted from the divider's current centre: reads as a converging feedback
    /// loop but is not one: `on_drag_move` fires per mouse event while `bounds`
    /// comes from the last laid-out frame, so a fast drag delivers several events
    /// against the same stale centre and applies the same correction several times
    /// over. That overshoots, then corrects back, which is the shimmer you see.
    ///
    /// The anchor is taken on mouse down, which is in window coordinates and
    /// happens before the drag threshold. `on_drag`'s constructor is the wrong
    /// place for it twice over: it fires only once the cursor has already
    /// travelled past `DRAG_THRESHOLD`, and the position it hands you is
    /// `cursor_offset`, the cursor's offset *within the 6px handle*, not a
    /// window coordinate.
    ///
    /// Dragging left widens the column, since the column's left edge is what moves.
    fn render_column_handle(&self, column: Column, cx: &Context<Self>) -> impl IntoElement + use<> {
        div()
            .id(("col-handle", column as usize))
            .w(px(COL_HANDLE_WIDTH))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(|style| style.bg(cx.theme().colors().border_selected))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    this.resize = Some(ColumnDrag {
                        column,
                        start_x: event.position.x,
                        start_width: this.widths.get(column),
                    });
                }),
            )
            .on_drag(
                ColumnResize {
                    pane: cx.entity_id(),
                    column,
                },
                |_, _, _, cx| cx.new(|_| EmptyDrag),
            )
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<ColumnResize>, _window, cx| {
                    // Whose drag this is, decided by the payload rather than by
                    // any state this pane happens to be holding.
                    let active = *event.drag(cx);
                    if active.pane != cx.entity_id() || active.column != column {
                        return;
                    }
                    let Some(drag) = this.resize.filter(|drag| drag.column == column) else {
                        return;
                    };
                    let moved = event.event.position.x - drag.start_x;
                    this.widths.set(column, drag.start_width - moved);
                    cx.emit(PaneEvent::ViewChanged);
                    cx.notify();
                },
            ))
    }

    /// Navigation toolbar: back / forward / up / home buttons plus the path.
    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        // The inactive pane's path drops to the muted colour, as Zed dims the
        // tab and breadcrumb of a pane that is not the focused one.
        let content = if self.active {
            colors.text
        } else {
            colors.text_muted
        };
        // Back, forward, up and home say exactly one thing: whether there is
        // anywhere to go. Dimming them with the pane as well would put two
        // meanings in one colour, and a disabled Back would then look identical
        // to a Back in a pane that simply is not focused.
        let nav_on = colors.text;
        let nav_off = colors.text_muted;
        let hover_bg = colors.element_hover;
        let searching = self.searching();

        let nav_button = |id: &'static str,
                          icon: &'static str,
                          enabled: bool,
                          action: Box<dyn gpui::Action>,
                          cx: &Context<Self>| {
            div()
                .id(id)
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .child(Icon::from_path(
                    icon,
                    if enabled { nav_on } else { nav_off },
                ))
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|s| s.bg(hover_bg))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            window.focus(&this.focus_handle, cx);
                            window.dispatch_action(action.boxed_clone(), cx);
                        }))
                })
        };

        div()
            .flex_none()
            .h(px(28.))
            .flex()
            .flex_row()
            .items_center()
            .gap_0p5()
            .px_1()
            .bg(colors.title_bar_background)
            .text_sm()
            .child(nav_button(
                "nav-back",
                "icons/file_icons/arrow_left.svg",
                self.history.can_back(),
                Box::new(NavBack),
                cx,
            ))
            .child(nav_button(
                "nav-forward",
                "icons/file_icons/arrow_right.svg",
                self.history.can_forward(),
                Box::new(NavForward),
                cx,
            ))
            .child(nav_button(
                "nav-up",
                "icons/file_icons/arrow_up.svg",
                self.dir.parent().is_some(),
                Box::new(GoUp),
                cx,
            ))
            .child(nav_button(
                "nav-home",
                "icons/file_icons/house.svg",
                std::env::var_os("HOME").map(PathBuf::from).as_deref() != self.dir.disk(),
                Box::new(GoHome),
                cx,
            ))
            .child(match self.path_editor.clone() {
                Some((_, editor)) => editor.into_any_element(),
                None => div()
                    .id("path-display-slot")
                    .flex_1()
                    .px_2()
                    .truncate()
                    .text_color(content)
                    .cursor_text()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.start_edit(window, cx);
                    }))
                    .child(self.dir.to_string())
                    .into_any_element(),
            })
            .child(
                div()
                    .id("hoja-search")
                    .flex_none()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    // Lit while a search is on, since the listing is then
                    // results rather than this directory and the toolbar is the
                    // only thing that says so.
                    .when(searching, |el| el.bg(colors.element_selected))
                    .hover(|s| s.bg(hover_bg))
                    .child(Icon::from_path(
                        "icons/file_icons/magnifying_glass.svg",
                        content,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            // A second press ends the search, matching escape.
                            if this.searching() {
                                this.end_search(window, cx);
                            } else {
                                this.start_search(&StartSearch, window, cx);
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .id("pane-menu")
                    .flex_none()
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg))
                    .child(Icon::from_path("icons/file_icons/menu.svg", content))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_settings_menu(event.position, window, cx);
                        }),
                    ),
            )
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        // Always muted, in both panes. Column labels are furniture: they say the
        // same thing every time you look, so they do not need to compete with
        // the names underneath them for attention.
        let content = colors.text_muted;
        let hover_bg = colors.element_hover;
        let sort = self.view.sort;

        // Clickable header cell. The resize handles are siblings, not ancestors, so
        // dragging a divider never lands a click on the cell beside it. `width` is
        // `None` for the flexible Name column.
        let head = |key: SortKey, label: &'static str, width: Option<Pixels>, right: bool| {
            let indicator = (sort.key == key).then_some(match sort.dir {
                SortDir::Ascending => "icons/file_icons/chevron_up.svg",
                SortDir::Descending => "icons/file_icons/chevron_down.svg",
            });

            div()
                .id(("col-head", key as usize))
                .h_full()
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .hover(|style| style.bg(hover_bg))
                // Held to the same edge as the figures under it. The sort
                // chevron follows the label, so it sits outermost.
                .when(right, |this| this.justify_end())
                .map(|this| match width {
                    Some(w) => this.w(w).flex_none(),
                    None => this.flex_1().min_w(px(NAME_MIN_WIDTH)),
                })
                .child(div().truncate().child(label))
                .children(indicator.map(|path| Icon::from_path(path, content).size(px(12.))))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.toggle_sort(key, cx);
                }))
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .bg(colors.elevated_surface_background)
            .border_b_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(content)
            .child(head(SortKey::Name, "Name", None, false));

        Column::ALL.into_iter().fold(header, |header, column| {
            header
                .child(self.render_column_handle(column, cx))
                .child(head(
                    column.sort_key(),
                    column.label(),
                    Some(self.widths.get(column)),
                    column.aligns_right(),
                ))
        })
    }

    /// `use<>` opts out of capturing the `&self` / `&Context` lifetimes: the returned
    /// element owns every value it needs, and `uniform_list`'s callback must return
    /// something with no borrows outstanding.
    fn render_entry(
        &self,
        ix: usize,
        now: SystemTime,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let entry = &self.entries[ix];
        let rename_editor = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.ix == ix)
            .map(|renaming| renaming.editor.clone());
        let selected = self.selected.contains(ix);
        // The lead row is where ctrl-arrow has moved to and what ctrl-space
        // acts on. It is usually also selected, so it needs its own mark.
        let is_lead = self.cursor_ix == Some(ix);
        let is_dir = entry.is_dir;
        // Dropping onto this row and dragging it away both end in a real file
        // operation, so both hang off this rather than off the key. `None`
        // means the row is a member of an archive, and neither is offered.
        let on_disk = entry.on_disk().map(Path::to_path_buf);

        let colors = cx.theme().colors();
        // Deliberately one content colour throughout: names, secondary columns, and
        // icons. No accent on directories, no muting on metadata: hierarchy comes from
        // column position and the selection background instead of from hue.
        //
        // Git status is the one exception, and only on the name. Hue here carries
        // information rather than decoration, which is the whole test: a green
        // name means untracked, and nothing else in the row says so. The
        // secondary columns and the icon stay neutral so the exception reads as
        // a signal instead of as theming.
        let base = colors.text;
        let name_base = self
            .git
            .get(&entry.name)
            .map(|status| match status {
                GitStatus::Added => colors.version_control_added,
                GitStatus::Modified => colors.version_control_modified,
                GitStatus::Deleted => colors.version_control_deleted,
                GitStatus::Renamed => colors.version_control_renamed,
                GitStatus::Conflict => colors.version_control_conflict,
                // `text_muted`, not `version_control_ignored`: the latter is
                // tuned for diff gutters where near-invisible is fine, and it
                // leaves a file name too dim to read. Ignored is a
                // de-emphasis, not a hue.
                GitStatus::Ignored => colors.text_muted,
                GitStatus::Unmodified => base,
            })
            .unwrap_or(base);

        // Icons and the secondary columns follow `content`; names carry their
        // own colour. Both quiet down together when this is not the pane the
        // keys are acting on.
        let content = quieted(base, self.active);
        let name_color = quieted(name_base, self.active);

        // Resolution order handles the messy cases: full filename against stems and
        // suffixes (`eslint.config.js`), repeated `split_once('.')` (`auth.module.js`),
        // multiple extensions (`Component.stories.tsx`), hidden files (`.gitignore`),
        // bare extension, then the `"default"` key.
        let icon_path = if is_dir {
            FileIcons::get_folder_icon(false, entry.key(), cx)
        } else {
            FileIcons::get_icon(entry.key(), cx)
        };

        // Cells line up with the header by using the same widths and the same
        // handle-sized spacers where the dividers sit.
        let spacer = || div().w(px(COL_HANDLE_WIDTH)).flex_none();
        let cell = move |cell: Cell| {
            let Cell {
                width,
                text,
                numeric,
                counting,
                right,
            } = cell;
            let body = div()
                .w(width)
                .flex_none()
                .px_2()
                .truncate()
                .text_color(content)
                .when(right, |el| el.text_right())
                // Digit under digit down the column, and the same shape as the
                // footer totalling them below.
                .when_some(numeric, |el, family| el.font_family(family));
            if !counting {
                return body.child(text).into_any_element();
            }
            // A bar rather than a number, because a number here would be a
            // number that is still climbing, and a listing full of those reads
            // as churn. It breathes instead: enough to say the row is waiting
            // on something, not enough to pull the eye down a column of them.
            // gpui holds it still for anyone who has asked for reduced motion.
            // The bar is an element rather than text, so it takes the cell's
            // alignment from a flex rule rather than from `text_right`.
            body.flex()
                .flex_row()
                .items_center()
                .when(right, |el| el.justify_end())
                .child(
                    div()
                        .w(px(COUNTING_BAR_WIDTH))
                        .h(px(COUNTING_BAR_HEIGHT))
                        .rounded_full()
                        .bg(content)
                        .with_animation(
                            ("counting", ix),
                            gpui::Animation::new(COUNTING_PERIOD).repeat().with_easing(
                                gpui::pulsating_between(COUNTING_ALPHA_LOW, COUNTING_ALPHA_HIGH),
                            ),
                            |bar, delta| bar.opacity(delta),
                        ),
                )
                .into_any_element()
        };

        // Resolved before the element is built: `use<>` forbids capturing `self`,
        // and this keeps the cells in lockstep with the header.
        // Resolved here with everything else the row needs, so nothing borrows
        // `self` past the end of this function.
        let folder_bytes = self.folder_bytes(ix);
        let numeric = crate::theming::numeric_font(cx);
        let counting = self.counting_size(ix);
        let cells = Column::ALL.map(|column| Cell {
            width: self.widths.get(column),
            text: column.value(entry, folder_bytes, now),
            numeric: column.is_numeric().then(|| numeric.clone()).flatten(),
            // Only the column that holds a folder size has anything to wait for.
            counting: counting && column == Column::Size,
            right: column.aligns_right(),
        });

        // A search result is labelled by where it sits; everything else by its
        // own name.
        let label = entry.relative.clone().unwrap_or_else(|| entry.name.clone());

        // Dragging a selected row takes the whole selection; dragging an
        // unselected one takes only itself, without disturbing the selection.
        //
        // `None` for a row with no real path, and the drag is then never
        // offered at all. Gating here rather than at the drop is deliberate: a
        // drag that starts and can land nowhere is worse than one that never
        // starts, and this payload can leave the window for another
        // application, which would otherwise be handed paths to nothing.
        let dragged = on_disk
            .clone()
            .zip(self.dir.disk().map(Path::to_path_buf))
            .map(|(anchor, source_dir)| DraggedPaths {
                resolved: std::cell::OnceCell::new(),
                pane: cx.entity(),
                anchor,
                whole_selection: selected,
                source_dir,
            });
        let drag_label: SharedString = if selected {
            match self.selected.len() {
                1 => entry.name.clone().into(),
                n => format!("{n} items").into(),
            }
        } else {
            entry.name.clone().into()
        };

        let row = div()
            .id(ix)
            // Uniform row height is what lets `uniform_list` virtualize.
            .h(px(ROW_HEIGHT))
            // `uniform_list` hands each row `Definite(list_width)` as *available* space,
            // but a flex root with `width: auto` sizes to its content, so without
            // `w_full` the Name cell's `flex_1` has nothing to expand into and the
            // columns drift out of alignment with the header.
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .cursor_pointer()
            .text_sm()
            // Every row carries the border, and only the lead one colours it:
            // uniform_list virtualizes on a single measured row height, so the
            // geometry cannot differ between rows.
            .border_1()
            .border_color(if is_lead {
                colors.border_focused
            } else {
                gpui::transparent_black()
            })
            .when(selected, |this| {
                // Held at half strength in the pane that is not taking keys, so
                // two panes with selections cannot be confused for each other.
                // Alpha rather than a different colour: it stays the selection
                // colour of whatever theme is loaded, only quieter.
                let mut fill = colors.element_selected;
                if !self.active {
                    fill.a *= INACTIVE_SELECTION_ALPHA;
                }
                this.bg(fill)
            })
            .when(!selected, |this| {
                this.hover(|style| style.bg(colors.element_hover))
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(NAME_MIN_WIDTH))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .text_color(name_color)
                    .children(icon_path.map(|path| Icon::from_path(path, content)))
                    .child(match rename_editor {
                        Some(editor) => editor.into_any_element(),
                        None => div().truncate().child(label).into_any_element(),
                    }),
            );

        cells
            .into_iter()
            .fold(row, |row, spec| row.child(spacer()).child(cell(spec)))
            // A folder is a place to drop things only when it is a folder on
            // the disk. There is nowhere to put a file inside an archive.
            .when_some(on_disk.clone().filter(|_| is_dir), |row, target| {
                let highlight = colors.drop_target_background;
                row.can_drop({
                    let target = target.clone();
                    move |dragged, _, _| drop_allowed(dragged, &target)
                })
                .drag_over::<DraggedPaths>(move |style, _, _, _| style.bg(highlight))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(highlight))
                .on_drop(cx.listener({
                    let target = target.clone();
                    move |this, dragged: &DraggedPaths, window, cx| {
                        let sources = dragged.paths();
                        this.accept_drop(
                            sources,
                            Some(&dragged.source_dir.clone()),
                            target.clone(),
                            window,
                            cx,
                        );
                    }
                }))
                .on_drop(cx.listener(
                    move |this, paths: &ExternalPaths, window, cx| {
                        this.accept_drop(paths.paths().to_vec(), None, target.clone(), window, cx);
                    },
                ))
            })
            .when_some(dragged, |row, dragged| {
                // Also where the payload settles what it carries: see `resolve`.
                row.on_drag(dragged, move |dragged: &DraggedPaths, _, _, cx| {
                    dragged.resolve(cx);
                    cx.new(|_| DragPreview {
                        label: drag_label.clone(),
                    })
                })
                // Promotes the same drag to a native one when it leaves the
                // window. The resolver runs once, at promotion, never per
                // frame, which is why the is_dir probes belong here and not in
                // the payload.
                .external_drag_payload(
                    |dragged: &DraggedPaths, _, _: &mut App| {
                        Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
                            dragged
                                .paths()
                                .into_iter()
                                .map(|path| (path.is_dir(), path))
                                .map(|(is_dir, path)| (path, is_dir)),
                        )))
                    },
                )
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // Right-click on an unselected row retargets the selection.
                    if !this.selected.contains(ix) {
                        this.selected.only(ix);
                        this.place_cursor(ix);
                    }
                    this.open_context_menu(event.position, true, window, cx);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                let mods = event.modifiers();
                if mods.control {
                    // Toggle membership; the toggled row becomes the new anchor.
                    this.selected.toggle(ix);
                    this.place_cursor(ix);
                } else if mods.shift {
                    // Range from the anchor replaces the selection; the anchor
                    // itself stays put so ranges can be re-aimed.
                    let a = this.anchor_ix.unwrap_or(ix);
                    let (lo, hi) = (a.min(ix), a.max(ix));
                    this.selected.set((lo..=hi).collect());
                    this.cursor_ix = Some(ix);
                } else {
                    this.selected.only(ix);
                    this.place_cursor(ix);
                    if event.click_count() >= 2 {
                        this.activate(ix, cx);
                    }
                }
                cx.notify();
            }))
    }

    /// The line at the bottom of the pane: what is selected, what the directory
    /// holds, or what a search is doing.
    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .px_2()
            .bg(colors.title_bar_background)
            .border_t_1()
            .border_color(colors.border)
            .text_xs()
            // Muted in both panes, like the column headers and for the same
            // reason: this is furniture. It says the same kind of thing every
            // time you look, and must not compete with the names above it.
            .text_color(colors.text_muted)
            // The same face as the Size column it totals, so "259 MB" here and
            // "259 MB" in a row above are the same width and the same shape.
            .when_some(crate::theming::numeric_font(cx), |el, family| {
                el.font_family(family)
            })
            .child(div().truncate().child(
                // A search is the pane's loudest running work, and while one
                // is on the rows are not this directory's, so nothing the
                // footer would otherwise say about them is true.
                self.footer_line(),
            ))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
                self.scroll.clone(),
                self.entries.len(),
                ROW_HEIGHT,
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Here rather than at each site that moves the selection: this runs
        // after every one of them, and cannot be forgotten by the next one.
        self.sync_footer(cx);

        // `None` inside an archive, and the pane then takes no drops at all:
        // there is nowhere in it to put a file.
        let here = self.dir.disk().map(Path::to_path_buf);
        let drop_border = cx.theme().colors().drop_target_background;

        div()
            .track_focus(&self.focus_handle)
            .key_context("DirPane")
            // The pane body is the fallback target: anything not dropped on a
            // folder row lands in the directory being shown. A folder row that
            // accepts consumes the drop first, so the two never both fire.
            .when_some(here, |pane, here| {
                pane.can_drop({
                    let here = here.clone();
                    move |dragged, _, _| drop_allowed(dragged, &here)
                })
                // A border rather than a fill: the whole pane going solid would
                // hide the listing you are aiming at.
                .drag_over::<DraggedPaths>(move |style, _, _, _| style.border_color(drop_border))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(drop_border))
                .on_drop(cx.listener({
                    let here = here.clone();
                    move |this, dragged: &DraggedPaths, window, cx| {
                        let sources = dragged.paths();
                        let source_dir = dragged.source_dir.clone();
                        this.accept_drop(sources, Some(&source_dir), here.clone(), window, cx);
                    }
                }))
                .on_drop(cx.listener(
                    move |this, paths: &ExternalPaths, window, cx| {
                        this.accept_drop(paths.paths().to_vec(), None, here.clone(), window, cx);
                    },
                ))
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
            .on_action(cx.listener(Self::refresh_action))
            .on_action(cx.listener(Self::reverse_sort))
            .on_action(cx.listener(Self::sort_by_name))
            .on_action(cx.listener(Self::sort_by_size))
            .on_action(cx.listener(Self::sort_by_kind))
            .on_action(cx.listener(Self::sort_by_modified))
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
            .on_action(cx.listener(Self::toggle_selection))
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
            .child(self.render_header(cx))
            .child(self.render_body(cx))
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
    fn only_kind_may_be_dragged_narrower_than_its_content() {
        // Kind alone: its content has no fixed length, so a floor sized to it
        // would be a guess. The other two hold figures of a known widest form
        // and their floors are those.
        assert_eq!(Column::Kind.min_width(), px(COL_MIN_WIDTH));
        for column in Column::ALL {
            assert!(f32::from(column.min_width()) <= f32::from(column.default_width()));
        }
    }
}
