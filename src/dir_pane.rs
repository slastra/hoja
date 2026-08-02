use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use file_icons::FileIcons;
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
use crate::path_editor::{PathEditor, PathEditorEvent};
use crate::icon::Icon;
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
const COL_MAX_WIDTH: f32 = 420.;
/// The Name column flexes to fill, but never below this.
const NAME_MIN_WIDTH: f32 = 100.;

/// Emitted upward to the workspace. The workspace turns `Focus` into "this is now the
/// active pane" — clicking a pane focuses it, and activation follows from that rather
/// than from separate click plumbing.
pub enum PaneEvent {
    Focus,
    /// Asks the workspace to remove this pane from the tree.
    #[allow(dead_code)]
    Remove,
    /// Something worth saying that has no job to attach to.
    Notice(String),
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
/// the drop rather than passing it to the element behind — which is what we
/// want, but is not a fall-through.
fn drop_allowed(dragged: &dyn std::any::Any, target: &Path, cx: &App) -> bool {
    if let Some(dragged) = dragged.downcast_ref::<DraggedPaths>() {
        return fs::is_valid_drop(&dragged.paths(cx), target);
    }
    if let Some(paths) = dragged.downcast_ref::<ExternalPaths>() {
        return fs::is_valid_drop(paths.paths(), target);
    }
    false
}

/// An open rename editor, anchored by name rather than by row.
///
/// A re-listing renumbers every row, so an index alone cannot survive one —
/// which is why a background copy finishing, or a delete in another pane, used
/// to close the editor out from under whoever was typing.
struct Renaming {
    name: String,
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

/// Rows being dragged, and where they came from so a drop back onto their own
/// directory can be refused.
///
/// The paths are shared rather than cloned: `on_drag` takes its value eagerly
/// at render time, so a plain `Vec` would copy the whole selection once per
/// visible row per frame.
#[derive(Clone)]
pub struct DraggedPaths {
    /// Resolved on drop rather than carried, because `on_drag` takes its value
    /// eagerly: gathering the selection for every visible row of every frame
    /// cost 3.9ms a frame with 100k rows selected, for a payload that almost
    /// every frame throws away.
    pane: Entity<DirPane>,
    /// The row the drag started on, used when it was not part of the selection.
    anchor: PathBuf,
    whole_selection: bool,
    pub source_dir: PathBuf,
}

impl DraggedPaths {
    pub fn paths(&self, cx: &App) -> Vec<PathBuf> {
        if self.whole_selection {
            self.pane.read(cx).selected_paths()
        } else {
            vec![self.anchor.clone()]
        }
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
/// Everything about a column lives here — its order, label, sort key, starting
/// width, and the text it shows — so a new column is one variant, one entry in
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

    fn default_width(self) -> Pixels {
        match self {
            Column::Size | Column::Kind => px(90.),
            Column::Modified => px(140.),
        }
    }

    /// The cell text for one entry. Empty where the entry has no such value —
    /// directories have no size, and unreadable metadata has no time.
    fn value(self, entry: &DirEntry) -> String {
        match self {
            Column::Size => entry.size.map(fs::format_size).unwrap_or_default(),
            Column::Kind => entry.kind(),
            Column::Modified => entry.modified.map(fs::format_time).unwrap_or_default(),
        }
    }
}

/// Per-column widths, positional so the set can grow without new fields.
///
/// Indexed by discriminant, not by position in `ALL`, so `ALL` must hold every
/// variant exactly once — otherwise a column indexes a width that belongs to
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
        self.0[column as usize] = width.clamp(px(COL_MIN_WIDTH), px(COL_MAX_WIDTH));
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

/// Marker type that `on_drag_move` dispatches on. Which column is being
/// resized lives in `DirPane::resize`, not here.
#[derive(Clone, Copy)]
struct ColumnResize;

/// Where a column resize started, captured once when the drag begins.
#[derive(Clone, Copy)]
struct ColumnDrag {
    column: Column,
    start_x: Pixels,
    start_width: Pixels,
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
    dir: PathBuf,
    history: History,
    entries: Vec<DirEntry>,
    /// Multi-select: plain click replaces, ctrl-click toggles, shift-click
    /// extends a range from the anchor, ctrl-a selects all.
    selected: BTreeSet<usize>,
    /// The row a shift-range extends from. Stays put while the lead moves.
    anchor_ix: Option<usize>,
    /// The lead row: what Rename, Open, and the context menu act on, and what
    /// the arrow keys move. Equal to `anchor_ix` except while a range is being
    /// extended — shift-down then shift-up has to *shrink* the range, which is
    /// only possible with the two ends tracked separately.
    cursor_ix: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Per-pane, because panes in a split can be very different widths.
    widths: ColumnWidths,
    /// Anchor for the resize in progress, if any.
    resize: Option<ColumnDrag>,
    /// Git status by entry name for the loaded directory. Empty outside a
    /// repository, and empty until the background query lands.
    git: crate::git::GitStatuses,
    git_task: Option<Task<()>>,
    /// The directory the change watcher is armed on, paired with its task, so
    /// re-listing the same directory does not tear down and rebuild the inotify
    /// watch — and so a watcher that failed to arm cannot be recorded as armed.
    watch: Option<(PathBuf, Task<()>)>,
    view: ViewSettings,
    /// The directory `entries` was built from. Differs from `dir` while a load
    /// is in flight, which is how a navigation is told apart from a refresh.
    loaded_dir: PathBuf,
    /// Set when the directory could not be read at all (permissions, gone, not a dir).
    error: Option<String>,
    /// Held so that navigating away cancels an in-flight read by dropping its task.
    load_task: Option<Task<()>>,
    /// Separate from `load_task` so a header click cannot cancel a pending read.
    sort_task: Option<Task<()>>,
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
    pending_select: Vec<String>,
    /// Type-ahead find: the last keystroke's time and the accumulated prefix.
    /// One value, so "no buffer" cannot disagree with "no timestamp".
    type_ahead: Option<(Instant, String)>,
    _subscriptions: Vec<Subscription>,
}

impl DirPane {
    pub fn new(
        dir: PathBuf,
        view: ViewSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let subscriptions = vec![cx.on_focus_in(&focus_handle, window, |_, _, cx| {
            cx.emit(PaneEvent::Focus);
        })];

        // Focus on creation so key bindings resolve without a click first.
        window.focus(&focus_handle, cx);

        let history = History::new(dir.clone());
        let mut this = Self {
            focus_handle,
            dir,
            history,
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor_ix: None,
            cursor_ix: None,
            scroll: UniformListScrollHandle::new(),
            widths: ColumnWidths::default(),
            resize: None,
            git: crate::git::GitStatuses::new(),
            git_task: None,
            watch: None,
            view,
            loaded_dir: PathBuf::new(),
            error: None,
            load_task: None,
            sort_task: None,
            context_menu: None,
            path_editor: None,
            search: None,
            renaming: None,
            pending_select: Vec::new(),
            type_ahead: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Splits copy the source pane's view settings, sort included.
    pub fn view_settings(&self) -> ViewSettings {
        self.view
    }

    fn toggle_hidden(&mut self, _: &ToggleHiddenFiles, _w: &mut Window, cx: &mut Context<Self>) {
        self.view.show_hidden = !self.view.show_hidden;
        self.relist(cx);
    }

    fn toggle_folders_first(
        &mut self,
        _: &ToggleFoldersFirst,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view.folders_first = !self.view.folders_first;
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

    /// Paths of the current selection, in listing order.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter_map(|&ix| self.entries.get(ix))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Re-read the directory (used by the workspace when a job completes here).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    /// Select these names once the next listing lands.
    pub fn select_on_next_load(&mut self, names: Vec<String>) {
        self.pending_select = names;
    }

    /// Aim the selection at whatever survives `removed`, then re-list.
    ///
    /// Deleting the row under the cursor and landing on nothing loses your
    /// place in a long listing, so the selection walks forward to the next
    /// survivor — or back to the previous one when the removed items were at
    /// the end. The listing still holds the departing entries at this point,
    /// which is what makes "next" meaningful.
    pub fn select_after_removal(&mut self, removed: &[PathBuf], cx: &mut Context<Self>) {
        let gone: std::collections::HashSet<&std::ffi::OsStr> =
            removed.iter().filter_map(|p| p.file_name()).collect();
        let surviving =
            |entry: &DirEntry| !gone.contains(std::ffi::OsStr::new(entry.name.as_str()));

        let first_gone = self
            .entries
            .iter()
            .position(|entry| !surviving(entry))
            .unwrap_or(0);
        let successor = self.entries[first_gone..]
            .iter()
            .find(|entry| surviving(entry))
            .or_else(|| self.entries[..first_gone].iter().rev().find(|e| surviving(e)));

        self.pending_select = successor.map(|entry| vec![entry.name.clone()]).unwrap_or_default();
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

        let mut items = Vec::new();
        // Where the "Open With" section lands once it resolves.
        let mut open_with_ix = None;
        if on_rows {
            items.push(MenuItem::action("Open", dispatch(Box::new(OpenSelected))));

            // MIME detection reads the head of the file, so it can block for as
            // long as the filesystem wants to take — unacceptable on a network
            // mount. The menu opens without this section and grows it in.
            let anchor_entry = self.cursor_ix.and_then(|ix| self.entries.get(ix));
            if let Some(entry) = anchor_entry.filter(|e| !e.is_dir) {
                open_with_ix = Some((items.len(), entry.path.clone()));
            }
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
            items.push(MenuItem::action("Copy", dispatch(Box::new(workspace::Copy))));
        }
        items.push(MenuItem::action("Paste", dispatch(Box::new(workspace::Paste))));
        items.push(MenuItem::Separator);
        items.push(MenuItem::action(
            "New Folder",
            dispatch(Box::new(workspace::NewFolder)),
        ));

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
            self.selected = BTreeSet::from([ix]);
            self.place_cursor(ix);
            self.scroll
                .scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
            cx.notify();
        }
        self.type_ahead = Some((now, buffer));
        true
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected = (0..self.entries.len()).collect();
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
    /// can tell where you came from — the convention in every list and editor.
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
        self.selected = BTreeSet::from([ix]);
        self.place_cursor(ix);
        self.reveal(ix, cx);
    }

    /// Move the lead and leave the selection alone, so a row can be added to a
    /// scattered selection without disturbing what is already in it. Pointless
    /// without the lead ring drawn in `render_row` — otherwise the next
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
        if !self.selected.remove(&ix) {
            self.selected.insert(ix);
        }
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
        let anchor = self.anchor_ix.filter(|&a| a < self.entries.len()).unwrap_or(ix);
        self.anchor_ix = Some(anchor);
        self.cursor_ix = Some(ix);
        self.selected = (anchor.min(ix)..=anchor.max(ix)).collect();
        self.reveal(ix, cx);
    }

    fn reveal(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Read the current directory on a background thread, then apply on the UI thread.
    ///
    /// Assigning to `self.load_task` drops any previous task, which cancels a read that
    /// is still running — so hammering navigation cannot land stale entries.
    /// Re-list without disturbing anything a re-listing cannot change.
    ///
    /// A view-setting change — hidden files, folder grouping — rebuilds the
    /// rows but cannot alter a file's git status, and re-asking costs two
    /// process spawns and a work-tree walk. It also blanks `self.git` first, so
    /// every name would flash back to its default colour and back again.
    fn relist(&mut self, cx: &mut Context<Self>) {
        self.reload_inner(false, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_inner(true, cx);
    }

    fn reload_inner(&mut self, refresh_git: bool, cx: &mut Context<Self>) {
        // A directory change starts fresh; a refresh of the same directory
        // keeps the selection.
        if self.pending_select.is_empty() && self.dir == self.loaded_dir {
            self.pending_select = self.selected_names();
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

        self.load_task = Some(cx.spawn(async move |this, cx| {
            // `read_dir` is blocking and sorting a large listing costs ~100ms, so both
            // run off the foreground executor in the same hop.
            let result = cx
                .background_spawn(async move {
                    let mut entries = fs::read_dir(&dir, show_hidden)?;
                    fs::sort_entries(&mut entries, sort, folders_first);
                    anyhow::Ok(entries)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.entries = entries;
                        this.error = None;
                    }
                    Err(err) => {
                        // A directory that has gone away is not a state to sit
                        // in: the pane would show a message with no way out but
                        // typing a path. Fall back to the nearest ancestor that
                        // still exists — the parent of a deleted folder, or the
                        // mount point's parent when a volume is unplugged.
                        //
                        // Only when it is *gone*. A directory that exists but
                        // cannot be read is a real, actionable error, and
                        // bouncing to the parent would hide it.
                        let gone = !this.dir.is_dir();
                        let fallback = gone
                            .then(|| fs::nearest_existing_dir(&this.dir))
                            .flatten()
                            .filter(|dir| dir != &this.dir);

                        if let Some(dir) = fallback {
                            cx.emit(PaneEvent::Notice(format!(
                                "{} is gone. Showing {}.",
                                this.dir.display(),
                                dir.display()
                            )));
                            this.navigate_to(dir, cx);
                            return;
                        }
                        this.entries.clear();
                        this.error = Some(err.to_string());
                    }
                }
                // Same directory: find the renamed row again. A navigation is
                // different — the same name elsewhere is a different file — so
                // the rename ends with the directory it belonged to.
                if this.dir == this.loaded_dir {
                    this.reanchor_rename();
                } else {
                    this.renaming = None;
                }
                this.loaded_dir = this.dir.clone();
                this.restore_selection();
                // Without this the mutation lands but nothing repaints.
                cx.notify();
            });
        }));
    }

    /// Names of the currently selected entries, in listing order.
    fn selected_names(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter_map(|&ix| self.entries.get(ix))
            .map(|e| e.name.clone())
            .collect()
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
            // copy — moving would delete data it still believes it owns. The
            // same translation also wipes the modifiers, so there is nothing to
            // read even if we wanted to offer a choice.
            None => Operation::Copy,
        };

        cx.emit(PaneEvent::Transfer {
            op,
            sources,
            dest,
        });
    }

    /// Re-list when something other than pane changes this directory.
    ///
    /// Without this the listing goes quietly stale: a file dragged out to
    /// another application, removed in a terminal, or written by a build all
    /// leave the pane showing entries that are no longer there. A file manager
    /// showing yesterday's truth is worse than a slow one.
    fn watch_dir(&mut self, cx: &mut Context<Self>) {
        if self.watch.as_ref().map(|(dir, _)| dir.as_path()) == Some(self.dir.as_path()) {
            return;
        }
        self.watch = None;
        let dir = self.dir.clone();

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

                let mut changed = false;
                while rx.try_recv().is_ok() {
                    changed = true;
                }
                if !changed {
                    continue;
                }

                // A copy or an extract emits one event per file. Coalesce the
                // burst so a long operation re-lists a few times rather than
                // thousands, which matters when the directory is large.
                cx.background_executor().timer(WATCH_INTERVAL).await;
                while rx.try_recv().is_ok() {}

                let alive = this.update(cx, |this, cx| this.reload(cx));
                if alive.is_err() {
                    break;
                }
            }
        });
        self.watch = Some((self.dir.clone(), task));
    }

    /// Ask git about this directory off the UI thread.
    ///
    /// `git status` spawns a process and walks the work tree — 142ms in a 17k
    /// file repository — so the listing never waits for it. The names colour in
    /// when the answer arrives. Assigning the task drops any previous one, so a
    /// fast sequence of navigations cannot land a stale answer on the wrong
    /// directory.
    fn reload_git(&mut self, cx: &mut Context<Self>) {
        // Clear first: the old directory's statuses must not tint this one.
        self.git = crate::git::GitStatuses::new();
        let dir = self.dir.clone();
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
    fn restore_selection(&mut self) {
        // A set, not the Vec: this runs once per entry, so a linear scan makes
        // restoring a large selection quadratic — measured at 11 seconds for
        // 100k rows selected, on the foreground executor.
        let wanted: std::collections::HashSet<String> =
            std::mem::take(&mut self.pending_select).into_iter().collect();
        self.selected.clear();
        self.anchor_ix = None;
        self.cursor_ix = None;
        if wanted.is_empty() {
            self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            return;
        }
        for (ix, entry) in self.entries.iter().enumerate() {
            if wanted.contains(&entry.name) {
                self.selected.insert(ix);
            }
        }
        self.anchor_ix = self.selected.iter().next().copied();
        self.cursor_ix = self.anchor_ix;
        match self.anchor_ix {
            Some(ix) => self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Nearest),
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
        if self.pending_select.is_empty() {
            self.pending_select = self.selected_names();
        }
        let sort = self.view.sort;
        let folders_first = self.view.folders_first;
        let mut entries = self.entries.clone();

        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn(async move {
                    fs::sort_entries(&mut entries, sort, folders_first);
                    entries
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.entries = entries;
                // The rows moved, so re-find them by name rather than index.
                this.renaming = None;
                this.restore_selection();
                this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn navigate_to(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if dir == self.dir {
            return;
        }
        self.history.push(dir.clone());
        self.load_dir_only(dir, cx);
    }

    /// Change directory without touching history — the back/forward path.
    fn load_dir_only(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.dir = dir;
        self.reload(cx);
        cx.notify();
    }

    fn nav_back(&mut self, _: &NavBack, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.history.back().map(Path::to_path_buf) {
            self.load_dir_only(dir, cx);
        }
    }

    fn nav_forward(&mut self, _: &NavForward, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.history.forward().map(Path::to_path_buf) {
            self.load_dir_only(dir, cx);
        }
    }

    fn rename_selected(
        &mut self,
        _: &RenameSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let entry_name = name.clone();
        let selection = fs::stem_range(&name);
        let editor =
            cx.new(|cx| PathEditor::new_with_selection(name, selection, window, cx));

        cx.subscribe_in(&editor, window, |this, editor, event, window, cx| match event {
            PathEditorEvent::Committed(text) => {
                let Some(ix) = this.renaming.as_ref().map(|r| r.ix) else {
                    return;
                };
                match this.commit_rename(ix, text) {
                    Ok(new_name) => {
                        this.renaming = None;
                        this.pending_select = vec![new_name];
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
        })
        .detach();

        let editor_focus = editor.focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                window.focus(&editor_focus, cx);
            });
        });
        self.renaming = Some(Renaming {
            name: entry_name,
            ix,
            editor,
        });
        cx.notify();
    }

    /// Returns the new name, or why the rename was refused. `RENAME_NOREPLACE`
    /// refuses to clobber an existing entry; filesystems without renameat2
    /// support fall back to an existence check plus a plain rename.
    fn commit_rename(&self, ix: usize, new_name: &str) -> Result<String, String> {
        let entry = self
            .entries
            .get(ix)
            .ok_or_else(|| "the entry is gone".to_string())?;
        let new_name = new_name.trim();
        if let Some(problem) = fs::name_problem(new_name) {
            return Err(problem.to_string());
        }
        if new_name == entry.name {
            return Ok(entry.name.clone());
        }
        let target = self.dir.join(new_name);
        hoja_transfer::rename_no_replace(&entry.path, &target)
            .map(|()| new_name.to_string())
            .map_err(|err| err.to_string())
    }

    fn edit_path(&mut self, _: &EditPath, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(window, cx);
    }

    fn start_search(&mut self, _: &StartSearch, window: &mut Window, cx: &mut Context<Self>) {
        // Reopening keeps whatever is already typed, so ctrl-f twice does not
        // silently throw the search away.
        let initial = self.search.as_ref().map(|s| s.query.clone()).unwrap_or_default();
        let editor = cx.new(|cx| {
            PathEditor::new(initial, window, cx).with_placeholder("Search this folder")
        });

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
                self.entries = previous.listing;
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
        self.clear_cursor();
        self.error = None;

        let dir = self.dir.clone();
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
                        search.handle =
                            Some(crate::search::spawn(dir, spawn_query, show_hidden));
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
                    let Some(handle) = this.search.as_ref().and_then(|s| s.handle.as_ref())
                    else {
                        return false;
                    };
                    let batch = handle.drain();
                    let finished = handle.is_done();
                    if !batch.is_empty() {
                        this.entries.extend(batch);
                        // Aim at the first hit as soon as there is one, so
                        // enter opens it without arrowing down first. Only
                        // while nothing is chosen — later batches must not
                        // yank the selection off what you picked.
                        if this.cursor_ix.is_none() && !this.entries.is_empty() {
                            this.selected = BTreeSet::from([0]);
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
    /// entry is gone — renamed or deleted by something else while it was open.
    fn reanchor_rename(&mut self) {
        let Some(name) = self.renaming.as_ref().map(|r| r.name.clone()) else {
            return;
        };
        match self.entries.iter().position(|entry| entry.name == name) {
            Some(ix) => {
                if let Some(renaming) = self.renaming.as_mut() {
                    renaming.ix = ix;
                }
            }
            None => self.renaming = None,
        }
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
    /// Shown in the workspace's status strip, which is where the pane's other
    /// running work already reports.
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

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            PathEditor::new(self.dir.display().to_string(), window, cx)
        });

        cx.subscribe_in(&editor, window, |this, editor, event, window, cx| match event {
            PathEditorEvent::Committed(text) => {
                match this.resolve_typed_path(text) {
                    Some(dir) => {
                        this.path_editor = None;
                        window.focus(&this.focus_handle, cx);
                        this.navigate_to(dir, cx);
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
        })
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
            self.dir.join(expanded)
        };
        absolute.is_dir().then_some(absolute)
    }

    fn go_home(&mut self, _: &GoHome, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            self.navigate_to(home, cx);
        }
    }

    fn go_up(&mut self, _: &GoUp, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            self.navigate_to(parent, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_ix.and_then(|ix| self.entries.get(ix)) else {
            return;
        };
        let path = entry.path.clone();
        if entry.is_dir {
            self.navigate_to(path, cx);
        } else if let Err(err) = crate::opener::open(&path) {
            eprintln!("[hoja] open failed: {err}");
        }
    }

    /// A divider sitting at the left edge of `column`.
    ///
    /// The width is computed *absolutely*, from the cursor's offset since the drag
    /// began. The obvious alternative — nudge the width by how far the cursor has
    /// drifted from the divider's current centre — reads as a converging feedback
    /// loop but is not one: `on_drag_move` fires per mouse event while `bounds`
    /// comes from the last laid-out frame, so a fast drag delivers several events
    /// against the same stale centre and applies the same correction several times
    /// over. That overshoots, then corrects back, which is the shimmer you see.
    ///
    /// The anchor is taken on mouse down, which is in window coordinates and
    /// happens before the drag threshold. `on_drag`'s constructor is the wrong
    /// place for it twice over: it fires only once the cursor has already
    /// travelled past `DRAG_THRESHOLD`, and the position it hands you is
    /// `cursor_offset` — the cursor's offset *within the 6px handle*, not a
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
            .on_drag(ColumnResize, |_, _, _, cx| cx.new(|_| EmptyDrag))
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<ColumnResize>, _window, cx| {
                    // `on_drag_move` dispatches on the payload *type*, so every handle
                    // sees every column drag. Without this guard, dragging one divider
                    // resizes all three.
                    let Some(drag) = this.resize.filter(|drag| drag.column == column) else {
                        return;
                    };
                    let moved = event.event.position.x - drag.start_x;
                    this.widths.set(column, drag.start_width - moved);
                    cx.notify();
                },
            ))
    }

    /// Navigation toolbar: back / forward / up / home buttons plus the path.
    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let content = colors.text;
        let muted = colors.text_muted;
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
                .child(Icon::from_path(icon, if enabled { content } else { muted }))
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
                std::env::var_os("HOME").map(PathBuf::from).as_deref() != Some(&self.dir),
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
                    .child(self.dir.display().to_string())
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
        let content = colors.text;
        let hover_bg = colors.element_hover;
        let sort = self.view.sort;

        // Clickable header cell. The resize handles are siblings, not ancestors, so
        // dragging a divider never lands a click on the cell beside it. `width` is
        // `None` for the flexible Name column.
        let head = |key: SortKey, label: &'static str, width: Option<Pixels>| {
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
            .child(head(SortKey::Name, "Name", None));

        Column::ALL.into_iter().fold(header, |header, column| {
            header
                .child(self.render_column_handle(column, cx))
                .child(head(
                    column.sort_key(),
                    column.label(),
                    Some(self.widths.get(column)),
                ))
        })
    }

    /// `use<>` opts out of capturing the `&self` / `&Context` lifetimes: the returned
    /// element owns every value it needs, and `uniform_list`'s callback must return
    /// something with no borrows outstanding.
    fn render_entry(&self, ix: usize, cx: &Context<Self>) -> impl IntoElement + use<> {
        let entry = &self.entries[ix];
        let rename_editor = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.ix == ix)
            .map(|renaming| renaming.editor.clone());
        let selected = self.selected.contains(&ix);
        // The lead row is where ctrl-arrow has moved to and what ctrl-space
        // acts on. It is usually also selected, so it needs its own mark.
        let is_lead = self.cursor_ix == Some(ix);
        let is_dir = entry.is_dir;
        let path = entry.path.clone();

        let colors = cx.theme().colors();
        // Deliberately one content colour throughout: names, secondary columns, and
        // icons. No accent on directories, no muting on metadata — hierarchy comes from
        // column position and the selection background instead of from hue.
        //
        // Git status is the one exception, and only on the name. Hue here carries
        // information rather than decoration, which is the whole test: a green
        // name means untracked, and nothing else in the row says so. The
        // secondary columns and the icon stay neutral so the exception reads as
        // a signal instead of as theming.
        let content = colors.text;
        let name_color = self
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
                GitStatus::Unmodified => content,
            })
            .unwrap_or(content);

        // Resolution order handles the messy cases: full filename against stems and
        // suffixes (`eslint.config.js`), repeated `split_once('.')` (`auth.module.js`),
        // multiple extensions (`Component.stories.tsx`), hidden files (`.gitignore`),
        // bare extension, then the `"default"` key.
        let icon_path = if is_dir {
            FileIcons::get_folder_icon(false, &entry.path, cx)
        } else {
            FileIcons::get_icon(&entry.path, cx)
        };

        // Cells line up with the header by using the same widths and the same
        // handle-sized spacers where the dividers sit.
        let spacer = || div().w(px(COL_HANDLE_WIDTH)).flex_none();
        let cell = move |width: Pixels, text: String| {
            div()
                .w(width)
                .flex_none()
                .px_2()
                .truncate()
                .text_color(content)
                .child(text)
        };

        // Resolved before the element is built: `use<>` forbids capturing `self`,
        // and this keeps the cells in lockstep with the header.
        let cells = Column::ALL.map(|column| (self.widths.get(column), column.value(entry)));

        // A search result is labelled by where it sits; everything else by its
        // own name.
        let label = entry.relative.clone().unwrap_or_else(|| entry.name.clone());

        // Dragging a selected row takes the whole selection; dragging an
        // unselected one takes only itself, without disturbing the selection.
        let dragged = DraggedPaths {
            pane: cx.entity(),
            anchor: entry.path.clone(),
            whole_selection: selected,
            source_dir: self.dir.clone(),
        };
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
            // but a flex root with `width: auto` sizes to its content — so without
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
            .when(selected, |this| this.bg(colors.element_selected))
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
            .fold(row, |row, (width, text)| {
                row.child(spacer()).child(cell(width, text))
            })
            .when(is_dir, |row| {
                let target = path.clone();
                let highlight = colors.drop_target_background;
                row.can_drop({
                    let target = target.clone();
                    move |dragged, _, cx| drop_allowed(dragged, &target, cx)
                })
                .drag_over::<DraggedPaths>(move |style, _, _, _| style.bg(highlight))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(highlight))
                .on_drop(cx.listener({
                    let target = target.clone();
                    move |this, dragged: &DraggedPaths, window, cx| {
                        let sources = dragged.paths(cx);
                        this.accept_drop(
                            sources,
                            Some(&dragged.source_dir.clone()),
                            target.clone(),
                            window,
                            cx,
                        );
                    }
                }))
                .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                    this.accept_drop(paths.paths().to_vec(), None, target.clone(), window, cx);
                }))
            })
            .on_drag(dragged, move |_, _, _, cx| {
                cx.new(|_| DragPreview {
                    label: drag_label.clone(),
                })
            })
            // Promotes the same drag to a native one when it leaves the window.
            // The resolver runs once, at promotion — never per frame — which is
            // why the is_dir probes belong here and not in the payload.
            .external_drag_payload(|dragged: &DraggedPaths, _, cx: &mut App| {
                Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
                    dragged
                        .paths(cx)
                        .into_iter()
                        .map(|path| (path.is_dir(), path))
                        .map(|(is_dir, path)| (path, is_dir)),
                )))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // Right-click on an unselected row retargets the selection.
                    if !this.selected.contains(&ix) {
                        this.selected = BTreeSet::from([ix]);
                        this.place_cursor(ix);
                    }
                    this.open_context_menu(event.position, true, window, cx);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                let mods = event.modifiers();
                if mods.control {
                    // Toggle membership; the toggled row becomes the new anchor.
                    if !this.selected.remove(&ix) {
                        this.selected.insert(ix);
                    }
                    this.place_cursor(ix);
                } else if mods.shift {
                    // Range from the anchor replaces the selection; the anchor
                    // itself stays put so ranges can be re-aimed.
                    let a = this.anchor_ix.unwrap_or(ix);
                    let (lo, hi) = (a.min(ix), a.max(ix));
                    this.selected = (lo..=hi).collect();
                    this.cursor_ix = Some(ix);
                } else {
                    this.selected = BTreeSet::from([ix]);
                    this.place_cursor(ix);
                    if event.click_count() >= 2 && is_dir {
                        this.navigate_to(path.clone(), cx);
                    }
                }
                cx.notify();
            }))
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

        uniform_list(
            "entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range.map(|ix| this.render_entry(ix, cx)).collect()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_1()
        .into_any_element()
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
        let here = self.dir.clone();
        let drop_border = cx.theme().colors().drop_target_background;

        div()
            .track_focus(&self.focus_handle)
            .key_context("DirPane")
            // The pane body is the fallback target: anything not dropped on a
            // folder row lands in the directory being shown. A folder row that
            // accepts consumes the drop first, so the two never both fire.
            .can_drop({
                let here = here.clone();
                move |dragged, _, cx| drop_allowed(dragged, &here, cx)
            })
            // A border rather than a fill: the whole pane going solid would hide
            // the listing you are aiming at.
            .drag_over::<DraggedPaths>(move |style, _, _, _| style.border_color(drop_border))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(drop_border))
            .on_drop(cx.listener({
                let here = here.clone();
                move |this, dragged: &DraggedPaths, window, cx| {
                    let sources = dragged.paths(cx);
                    let source_dir = dragged.source_dir.clone();
                    this.accept_drop(sources, Some(&source_dir), here.clone(), window, cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                this.accept_drop(paths.paths().to_vec(), None, here.clone(), window, cx);
            }))
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
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
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
            }))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(Motion::Up, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(Motion::Down, cx)))
            .on_action(cx.listener(|this, _: &MovePageUp, _, cx| {
                this.move_cursor(Motion::PageUp, cx)
            }))
            .on_action(cx.listener(|this, _: &MovePageDown, _, cx| {
                this.move_cursor(Motion::PageDown, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveToTop, _, cx| this.move_cursor(Motion::Top, cx)))
            .on_action(cx.listener(|this, _: &MoveToBottom, _, cx| {
                this.move_cursor(Motion::Bottom, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| this.extend_cursor(Motion::Up, cx)))
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| {
                this.extend_cursor(Motion::Down, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendPageUp, _, cx| {
                this.extend_cursor(Motion::PageUp, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendPageDown, _, cx| {
                this.extend_cursor(Motion::PageDown, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendToTop, _, cx| {
                this.extend_cursor(Motion::Top, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendToBottom, _, cx| {
                this.extend_cursor(Motion::Bottom, cx)
            }))
            .on_action(cx.listener(|this, _: &CursorUp, _, cx| this.focus_cursor(Motion::Up, cx)))
            .on_action(cx.listener(|this, _: &CursorDown, _, cx| {
                this.focus_cursor(Motion::Down, cx)
            }))
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
