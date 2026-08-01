use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use file_icons::FileIcons;
use gpui::{
    App, ClickEvent, Context, DismissEvent, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, Pixels, Point, Subscription, Task,
    UniformListScrollHandle, Window, actions, anchored, deferred, div, prelude::*, px,
    uniform_list,
};
use theme::ActiveTheme;

use crate::file_menu::{FileMenu, MenuItem};
use crate::fs::{self, DirEntry, Sort, SortDir, SortKey, ViewSettings};
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
}

/// The three fixed-width columns. Name is not listed because it flexes to fill whatever
/// space these leave, so it is never resized directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Size,
    Kind,
    Modified,
}

#[derive(Clone, Copy, Debug)]
struct ColumnWidths {
    size: Pixels,
    kind: Pixels,
    modified: Pixels,
}

impl Default for ColumnWidths {
    fn default() -> Self {
        Self {
            size: px(90.),
            kind: px(90.),
            modified: px(140.),
        }
    }
}

impl ColumnWidths {
    fn get(&self, column: Column) -> Pixels {
        match column {
            Column::Size => self.size,
            Column::Kind => self.kind,
            Column::Modified => self.modified,
        }
    }

    fn set(&mut self, column: Column, width: Pixels) {
        let slot = match column {
            Column::Size => &mut self.size,
            Column::Kind => &mut self.kind,
            Column::Modified => &mut self.modified,
        };
        *slot = width.clamp(px(COL_MIN_WIDTH), px(COL_MAX_WIDTH));
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
    path_editor: Option<Entity<PathEditor>>,
    /// Inline rename: the row index and its editor.
    renaming: Option<(usize, Entity<PathEditor>)>,
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
            view,
            loaded_dir: PathBuf::new(),
            error: None,
            load_task: None,
            sort_task: None,
            context_menu: None,
            path_editor: None,
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
        self.reload(cx);
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
        let gone: Vec<&std::ffi::OsStr> = removed.iter().filter_map(|p| p.file_name()).collect();
        let surviving = |entry: &DirEntry| !gone.iter().any(|name| *name == entry.name.as_str());

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
                            eprintln!("[pane] launch failed: {err}");
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

    fn clear_selection(&mut self, _: &ClearSelection, _window: &mut Window, cx: &mut Context<Self>) {
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
    fn reload(&mut self, cx: &mut Context<Self>) {
        // A directory change starts fresh; a refresh of the same directory
        // keeps the selection.
        if self.pending_select.is_empty() && self.dir == self.loaded_dir {
            self.pending_select = self.selected_names();
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
                        this.entries.clear();
                        this.error = Some(err.to_string());
                    }
                }
                this.renaming = None;
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

    /// Re-select `pending_select` against the freshly built listing and scroll
    /// the first survivor into view. Entries that disappeared are dropped.
    fn restore_selection(&mut self) {
        let wanted = std::mem::take(&mut self.pending_select);
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
        let selection = fs::stem_range(&name);
        let editor =
            cx.new(|cx| PathEditor::new_with_selection(name, selection, window, cx));

        cx.subscribe_in(&editor, window, |this, editor, event, window, cx| match event {
            PathEditorEvent::Committed(text) => {
                let Some((ix, _)) = this.renaming.clone() else {
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
        self.renaming = Some((ix, editor));
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
        pane_transfer::rename_no_replace(&entry.path, &target)
            .map(|()| new_name.to_string())
            .map_err(|err| err.to_string())
    }

    fn edit_path(&mut self, _: &EditPath, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(window, cx);
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
            PathEditorEvent::Cancelled => {
                this.path_editor = None;
                window.focus(&this.focus_handle, cx);
                cx.notify();
            }
        })
        .detach();

        window.focus(&editor.focus_handle(cx), cx);
        self.path_editor = Some(editor);
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
            eprintln!("[pane] open failed: {err}");
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
                Some(editor) => editor.into_any_element(),
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
        // dragging a divider never lands a click on the cell beside it.
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

        div()
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
            .child(head(SortKey::Name, "Name", None))
            .child(self.render_column_handle(Column::Size, cx))
            .child(head(SortKey::Size, "Size", Some(self.widths.size)))
            .child(self.render_column_handle(Column::Kind, cx))
            .child(head(SortKey::Kind, "Kind", Some(self.widths.kind)))
            .child(self.render_column_handle(Column::Modified, cx))
            .child(head(
                SortKey::Modified,
                "Modified",
                Some(self.widths.modified),
            ))
    }

    /// `use<>` opts out of capturing the `&self` / `&Context` lifetimes: the returned
    /// element owns every value it needs, and `uniform_list`'s callback must return
    /// something with no borrows outstanding.
    fn render_entry(&self, ix: usize, cx: &Context<Self>) -> impl IntoElement + use<> {
        let entry = &self.entries[ix];
        let rename_editor = self
            .renaming
            .as_ref()
            .filter(|(renaming_ix, _)| *renaming_ix == ix)
            .map(|(_, editor)| editor.clone());
        let selected = self.selected.contains(&ix);
        // The lead row is where ctrl-arrow has moved to and what ctrl-space
        // acts on. It is usually also selected, so it needs its own mark.
        let is_lead = self.cursor_ix == Some(ix);
        let is_dir = entry.is_dir;
        let path = entry.path.clone();

        let size_label = match entry.size {
            Some(bytes) => fs::format_size(bytes),
            None => String::new(),
        };
        let modified_label = entry.modified.map(fs::format_time).unwrap_or_default();

        let colors = cx.theme().colors();
        // Deliberately one content colour throughout: names, secondary columns, and
        // icons. No accent on directories, no muting on metadata — hierarchy comes from
        // column position and the selection background instead of from hue.
        let content = colors.text;

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

        div()
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
                    .text_color(content)
                    .children(icon_path.map(|path| Icon::from_path(path, content)))
                    .child(match rename_editor {
                        Some(editor) => editor.into_any_element(),
                        None => div()
                            .truncate()
                            .child(entry.name.clone())
                            .into_any_element(),
                    }),
            )
            .child(spacer())
            .child(cell(self.widths.size, size_label))
            .child(spacer())
            .child(cell(self.widths.kind, entry.kind()))
            .child(spacer())
            .child(cell(self.widths.modified, modified_label))
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
        div()
            .track_focus(&self.focus_handle)
            .key_context("DirPane")
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::nav_back))
            .on_action(cx.listener(Self::nav_forward))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::edit_path))
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
