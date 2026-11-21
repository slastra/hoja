//! The pane's menus: the context menu on an entry or on empty space, and
//! the view menu behind the toolbar button.

use super::*;

impl DirPane {
    /// Summon the context menu at `position` (window coords, from the mouse event).
    pub(super) fn open_context_menu(
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
            } else {
                // Copying out is the one selection-acting operation that
                // works on an archive; rename, delete and cut would all just
                // refuse (`stash_selection` says so explicitly for cut), so
                // they stay absent, matching the rule above, rather than
                // present and refusing.
                items.push(MenuItem::Separator);
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
    pub(super) fn resolve_open_with(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
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
                // One row that opens a menu, not a flat run. Twelve applications
                // register for a PNG on a well-stocked machine, which swamped the
                // menu they were part of; the old `.take(8)` kept it survivable by
                // dropping four of them with nothing on screen to say so.
                let apps = apps
                    .into_iter()
                    .map(|app| {
                        MenuItem::action(app.name.clone(), move |_, _| {
                            if let Err(err) = crate::opener::launch(&app) {
                                eprintln!("[hoja] launch failed: {err}");
                            }
                        })
                    })
                    .collect();
                let items = vec![MenuItem::submenu("Open With", apps)];
                menu.update(cx, |menu, cx| menu.insert_items(ix, items, cx));
            })
            .ok();
        })
        .detach();
    }

    /// The rows of the pane view menu.
    ///
    /// Separate from opening it because the Columns toggles leave the menu
    /// standing, and a check mark that does not follow the thing it reports is
    /// worse than a menu that closes. `toggle_column` builds these again and
    /// hands them back to the open menu.
    pub(super) fn settings_menu_items(&self, cx: &mut Context<Self>) -> Vec<MenuItem> {
        let pane_focus = self.focus_handle.clone();
        let dispatch = move |action: Box<dyn gpui::Action>| {
            let pane_focus = pane_focus.clone();
            move |window: &mut Window, cx: &mut App| {
                window.focus(&pane_focus, cx);
                window.dispatch_action(action.boxed_clone(), cx);
            }
        };

        // Straight to this pane rather than through an action, which the rows
        // around it use: six actions whose only caller is one submenu row
        // would be six more entries in the command palette saying what the
        // submenu already says.
        //
        // And without `dispatch`'s focus step, deliberately. Focusing the pane
        // blurs the menu, which dismisses it, and these are the rows that stay
        // open so the next column is one keystroke away. Focus comes back to
        // the pane when the menu really is dismissed; `show_menu` subscribes
        // for exactly that.
        let toggle_column = {
            let pane = cx.entity().downgrade();
            move |column: Column| {
                let pane = pane.clone();
                move |_: &mut Window, cx: &mut App| {
                    pane.update(cx, |pane, cx| pane.toggle_column(column, cx))
                        .ok();
                }
            }
        };

        // The details the layout in force draws: the list's columns, or what
        // each card shows. One submenu edits whichever it is.
        let (shown, details_label) = match self.view.layout {
            fs::Layout::List => (self.view.columns, "Columns"),
            fs::Layout::Grid => (self.view.card_details, "Card Details"),
        };

        let mut items = vec![
            MenuItem::toggle(
                "List",
                self.view.layout == fs::Layout::List,
                dispatch(Box::new(ListView)),
            ),
            MenuItem::toggle(
                "Grid",
                self.view.layout == fs::Layout::Grid,
                dispatch(Box::new(GridView)),
            ),
            MenuItem::Separator,
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
            MenuItem::submenu(
                details_label,
                Column::ALL
                    .into_iter()
                    .map(|column| {
                        MenuItem::toggle(column.label(), shown.get(column), toggle_column(column))
                    })
                    .collect(),
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
        ];

        // Only for the columns this pane draws. Sorting by a hidden column is
        // still legitimate, and the palette can still ask for it, but offering
        // it here beside a column nobody can see is how a short menu becomes a
        // long one. The four above are always offered because Name is always
        // drawn and the other three are the set that has always been there.
        for (column, action) in [
            (
                Column::Permissions,
                Box::new(SortByPermissions) as Box<dyn gpui::Action>,
            ),
            (Column::Owner, Box::new(SortByOwner)),
            (Column::Group, Box::new(SortByGroup)),
        ] {
            // Offered when the column is drawn, and also when the listing is
            // sorted by it whether it is drawn or not. Hiding a column does not
            // change the order, and the chevron goes with the column, so
            // without this second case nothing on screen names the order the
            // rows are actually in.
            if shown.get(column) || self.view.sort.key == column.sort_key() {
                items.push(MenuItem::toggle(
                    format!("Sort by {}", column.label()),
                    self.view.sort.key == column.sort_key(),
                    dispatch(action),
                ));
            }
        }

        items.extend([
            MenuItem::Separator,
            MenuItem::toggle(
                "Reverse Order",
                self.view.sort.dir == SortDir::Descending,
                dispatch(Box::new(ReverseSort)),
            ),
            MenuItem::Separator,
            MenuItem::action("Refresh", dispatch(Box::new(Refresh))),
        ]);

        items
    }

    /// The pane view menu behind the hamburger button.
    pub(super) fn open_settings_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.settings_menu_items(cx);
        self.show_menu(items, position, window, cx);
    }

    /// Shared summoning for the context and settings menus: build, subscribe,
    /// deferred focus, store in the single overlay slot.
    pub(super) fn show_menu(
        &mut self,
        items: Vec<MenuItem>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A right-click with nothing to offer, an archive's empty space being
        // the one place this happens today, gets no menu rather than an empty
        // one: the same "shorter menu over one that all says no" reasoning
        // `open_context_menu` already applies to individual items, one level
        // further out.
        if items.is_empty() {
            return;
        }

        let menu = cx.new(|cx| FileMenu::new(items, position, window, cx));

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
        //
        // Only if this is still the open menu. An Escape in the gap closes it
        // (see `clear_selection`), and focus handed to a menu no longer drawn
        // would leave the keys going nowhere.
        let menu_focus = menu.focus_handle(cx);
        let (pane, menu_id) = (cx.entity().downgrade(), menu.entity_id());
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let still_open = pane
                    .read_with(cx, |pane, _| {
                        pane.context_menu
                            .as_ref()
                            .is_some_and(|(_, menu)| menu.entity_id() == menu_id)
                    })
                    .unwrap_or(false);
                if still_open {
                    window.focus(&menu_focus, cx);
                }
            });
        });

        self.context_menu = Some((position, menu));
        cx.notify();
    }
}
