use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use file_icons::FileIcons;
use gpui::{
    App, ClickEvent, Context, DismissEvent, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, Pixels, Point, Subscription, Task,
    UniformListScrollHandle, Window, actions, anchored, deferred, div, prelude::*, px,
    uniform_list,
};
use theme::ActiveTheme;

use crate::file_menu::{FileMenu, MenuItem};
use crate::fs::{self, DirEntry, Sort, SortDir, SortKey};
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
        NavBack,
        NavForward,
        GoHome,
        EditPath,
        RenameSelected,
        ToggleHiddenFiles,
        ToggleFoldersFirst,
        Refresh,
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
    fn adjust(&mut self, column: Column, delta: Pixels) {
        let slot = match column {
            Column::Size => &mut self.size,
            Column::Kind => &mut self.kind,
            Column::Modified => &mut self.modified,
        };
        *slot = (*slot + delta).clamp(px(COL_MIN_WIDTH), px(COL_MAX_WIDTH));
    }
}

/// Drag payload identifying which column a divider resizes.
#[derive(Clone, Copy)]
struct ColumnResize(Column);

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
    /// The row a shift-range extends from: last plain- or ctrl-clicked row.
    anchor_ix: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Per-pane, because panes in a split can be very different widths.
    widths: ColumnWidths,
    sort: Sort,
    show_hidden: bool,
    folders_first: bool,
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
    /// After a reload, select the entry with this name (used to keep the
    /// renamed entry selected once the listing refreshes).
    pending_select: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl DirPane {
    pub fn new(dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            scroll: UniformListScrollHandle::new(),
            widths: ColumnWidths::default(),
            sort: Sort::default(),
            show_hidden: false,
            folders_first: true,
            error: None,
            load_task: None,
            sort_task: None,
            context_menu: None,
            path_editor: None,
            renaming: None,
            pending_select: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Splits copy the source pane's view settings.
    pub fn view_settings(&self) -> (bool, bool) {
        (self.show_hidden, self.folders_first)
    }

    pub fn set_view_settings(
        &mut self,
        show_hidden: bool,
        folders_first: bool,
        cx: &mut Context<Self>,
    ) {
        if (self.show_hidden, self.folders_first) != (show_hidden, folders_first) {
            self.show_hidden = show_hidden;
            self.folders_first = folders_first;
            self.reload(cx);
        }
    }

    fn toggle_hidden(&mut self, _: &ToggleHiddenFiles, _w: &mut Window, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.reload(cx);
    }

    fn toggle_folders_first(
        &mut self,
        _: &ToggleFoldersFirst,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.folders_first = !self.folders_first;
        self.apply_sort(cx);
    }

    fn refresh_action(&mut self, _: &Refresh, _w: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn sort_by_name(&mut self, _: &SortByName, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_sort(SortKey::Name, cx);
    }
    fn sort_by_size(&mut self, _: &SortBySize, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_sort(SortKey::Size, cx);
    }
    fn sort_by_kind(&mut self, _: &SortByKind, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_sort(SortKey::Kind, cx);
    }
    fn sort_by_modified(&mut self, _: &SortByModified, _w: &mut Window, cx: &mut Context<Self>) {
        self.set_sort(SortKey::Modified, cx);
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
        if on_rows {
            items.push(MenuItem::action("Open", dispatch(Box::new(OpenSelected))));

            // Flat "Open With" section: the registered apps for the anchored
            // file's MIME type, launched directly (no action round-trip).
            let anchor_entry = self.anchor_ix.and_then(|ix| self.entries.get(ix));
            if let Some(entry) = anchor_entry.filter(|e| !e.is_dir) {
                let apps = crate::opener::apps_for(&entry.path);
                if !apps.is_empty() {
                    items.push(MenuItem::Separator);
                }
                for app in apps.into_iter().take(8) {
                    let label = format!("Open with {}", app.name);
                    items.push(MenuItem::action(label, move |_, _| {
                        if let Err(err) = crate::opener::launch(&app) {
                            eprintln!("[pane] launch failed: {err}");
                        }
                    }));
                }
            }
            items.push(MenuItem::Separator);
            items.push(MenuItem::action(
                "Rename",
                dispatch(Box::new(RenameSelected)),
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

        let menu = cx.new(|cx| FileMenu::new(items, window, cx));

        cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.context_menu = None;
            // A menu item may have started an inline edit (Rename, path edit);
            // refocusing the pane here would stomp the editor's focus.
            if this.renaming.is_none() && this.path_editor.is_none() {
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
                self.show_hidden,
                dispatch(Box::new(ToggleHiddenFiles)),
            ),
            MenuItem::toggle(
                "Folders First",
                self.folders_first,
                dispatch(Box::new(ToggleFoldersFirst)),
            ),
            MenuItem::Separator,
            MenuItem::toggle(
                "Sort by Name",
                self.sort.key == SortKey::Name,
                dispatch(Box::new(SortByName)),
            ),
            MenuItem::toggle(
                "Sort by Size",
                self.sort.key == SortKey::Size,
                dispatch(Box::new(SortBySize)),
            ),
            MenuItem::toggle(
                "Sort by Kind",
                self.sort.key == SortKey::Kind,
                dispatch(Box::new(SortByKind)),
            ),
            MenuItem::toggle(
                "Sort by Modified",
                self.sort.key == SortKey::Modified,
                dispatch(Box::new(SortByModified)),
            ),
            MenuItem::Separator,
            MenuItem::action("Refresh", dispatch(Box::new(Refresh))),
        ];

        self.show_menu(items, position, window, cx);
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
            if this.renaming.is_none() && this.path_editor.is_none() {
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

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected = (0..self.entries.len()).collect();
        if self.anchor_ix.is_none() && !self.entries.is_empty() {
            self.anchor_ix = Some(0);
        }
        cx.notify();
    }

    fn clear_selection(&mut self, _: &ClearSelection, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected.clear();
        self.anchor_ix = None;
        cx.notify();
    }

    /// Read the current directory on a background thread, then apply on the UI thread.
    ///
    /// Assigning to `self.load_task` drops any previous task, which cancels a read that
    /// is still running — so hammering navigation cannot land stale entries.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let dir = self.dir.clone();
        let sort = self.sort;
        let show_hidden = self.show_hidden;
        let folders_first = self.folders_first;

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
                this.selected.clear();
                this.anchor_ix = None;
                this.renaming = None;
                if let Some(name) = this.pending_select.take()
                    && let Some(ix) = this.entries.iter().position(|e| e.name == name)
                {
                    this.selected.insert(ix);
                    this.anchor_ix = Some(ix);
                    this.scroll
                        .scroll_to_item(ix, gpui::ScrollStrategy::Center);
                } else {
                    this.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                }
                // Without this the mutation lands but nothing repaints.
                cx.notify();
            });
        }));
    }

    /// Header click: same column flips direction, a new column adopts its natural
    /// starting direction.
    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = if self.sort.key == key {
            Sort {
                key,
                dir: self.sort.dir.toggled(),
            }
        } else {
            Sort {
                key,
                // Time starts newest-first; everything else starts ascending. Clicking
                // "Modified" and getting 2019 first would be nobody's intent.
                dir: match key {
                    SortKey::Modified => SortDir::Descending,
                    _ => SortDir::Ascending,
                },
            }
        };
        self.apply_sort(cx);
    }

    /// Re-order the listing already in memory, without re-reading the directory.
    ///
    /// Measured at ~90-150ms for 100k entries in a debug build, which is well past the
    /// frame budget, so it runs on the background executor. The current list stays on
    /// screen until the sorted one arrives rather than blanking.
    fn apply_sort(&mut self, cx: &mut Context<Self>) {
        let sort = self.sort;
        let folders_first = self.folders_first;
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
                // Selected rows are almost certainly somewhere else now.
                this.selected.clear();
                this.anchor_ix = None;
                this.renaming = None;
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
        let Some(ix) = self.anchor_ix.filter(|&ix| ix < self.entries.len()) else {
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
                        this.pending_select = Some(new_name);
                        window.focus(&this.focus_handle, cx);
                        this.reload(cx);
                    }
                    Err(_) => {
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

    /// Returns the new name on success. `RENAME_NOREPLACE` refuses to clobber
    /// an existing entry; filesystems without renameat2 support fall back to an
    /// existence check plus a plain rename.
    fn commit_rename(&self, ix: usize, new_name: &str) -> Result<String, ()> {
        let entry = self.entries.get(ix).ok_or(())?;
        let new_name = new_name.trim();
        if fs::name_problem(new_name).is_some() {
            return Err(());
        }
        if new_name == entry.name {
            return Ok(entry.name.clone()); // no-op rename is a success
        }
        let target = self.dir.join(new_name);
        pane_transfer::rename_no_replace(&entry.path, &target)
            .map(|()| new_name.to_string())
            .map_err(|_| ())
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
        let Some(entry) = self.anchor_ix.and_then(|ix| self.entries.get(ix)) else {
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
    /// Resizing is incremental: each move nudges the width by how far the cursor has
    /// drifted from the divider's own current centre. Because the divider is re-laid-out
    /// every frame at the new width, this converges on the cursor instead of needing the
    /// drag's start position, which `on_drag` cannot capture.
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
            .on_drag(ColumnResize(column), |_, _, _, cx| cx.new(|_| EmptyDrag))
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<ColumnResize>, _window, cx| {
                    // `on_drag_move` dispatches on the payload *type*, so every handle
                    // sees every column drag. Without this guard, dragging one divider
                    // resizes all three.
                    if event.drag(cx).0 != column {
                        return;
                    }
                    let drift = event.event.position.x - event.bounds.center().x;
                    this.widths.adjust(column, -drift);
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
        let sort = self.sort;

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
                    this.set_sort(key, cx);
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
                        this.anchor_ix = Some(ix);
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
                    this.anchor_ix = Some(ix);
                } else if mods.shift {
                    // Range from the anchor replaces the selection; the anchor
                    // itself stays put so ranges can be re-aimed.
                    let a = this.anchor_ix.unwrap_or(ix);
                    let (lo, hi) = (a.min(ix), a.max(ix));
                    this.selected = (lo..=hi).collect();
                } else {
                    this.selected = BTreeSet::from([ix]);
                    this.anchor_ix = Some(ix);
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
            .on_action(cx.listener(Self::sort_by_name))
            .on_action(cx.listener(Self::sort_by_size))
            .on_action(cx.listener(Self::sort_by_kind))
            .on_action(cx.listener(Self::sort_by_modified))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_selection))
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
