//! What an entry does under the pointer: the drags out of a pane, the drops
//! into one, and `DirPane::interactive`, which wires a row or a card for both.

use super::*;

/// Whether a drag may land on `target`, for either payload type.
///
/// `can_drop` runs before the drop and also gates the highlight, so refusing
/// here means an illegal target never lights up. Note that a refusal *consumes*
/// the drop rather than passing it to the element behind, which is what we
/// want, but is not a fall-through.
pub(super) fn drop_allowed(dragged: &dyn std::any::Any, target: &Path) -> bool {
    if let Some(dragged) = dragged.downcast_ref::<DraggedPaths>() {
        return fs::is_valid_drop(&dragged.paths(), target);
    }
    if let Some(paths) = dragged.downcast_ref::<ExternalPaths>() {
        return fs::is_valid_drop(paths.paths(), target);
    }
    // An archive member has no path of its own for `is_valid_drop`'s
    // not-into-itself check to run against, and every real directory this was
    // ever offered to (the row-level and pane-level targets both already
    // require one) is a legal place to copy one out to.
    dragged.downcast_ref::<DraggedMembers>().is_some()
}

/// Say whether a drop will be taken, in colour.
///
/// The cursor used to carry this alone, and cannot any more: a drag that the
/// compositor is carrying owns the pointer, so `set_cursor` is ignored for the
/// whole gesture. Painting the target says the same thing and keeps saying it
/// after a round trip out of the window, and for drags arriving from other
/// applications, both of which the cursor never managed.
pub(super) fn tint(allowed: bool, accept: gpui::Hsla, refuse: gpui::Hsla) -> gpui::Hsla {
    if allowed { accept } else { refuse }
}

/// Point the cursor at whether a drop here would be taken or refused.
///
/// `target` is the directory a drop would land in, or `None` where there is no
/// such place at all, which is what an archive is.
///
/// Only while the pointer is actually inside `bounds`: `on_drag_move` fires for
/// **every** element that registered one, wherever the pointer is, so without
/// this test a row on the far side of the window sets the cursor for a pointer
/// nowhere near it. `bounds` is the last laid-out frame's, the same staleness
/// the column drag documents above, and harmless for a question this coarse.
pub(super) fn point_at_drop(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    dragged: &dyn std::any::Any,
    target: Option<&Path>,
    window: &mut Window,
    cx: &mut App,
) {
    if !bounds.contains(&position) {
        return;
    }
    let wanted = match target {
        Some(dir) if drop_allowed(dragged, dir) => CursorStyle::Arrow,
        // Refused, and an archive reaches this arm by having no directory at
        // all rather than by failing the predicate.
        _ => CursorStyle::OperationNotAllowed,
    };
    // `set_active_drag_cursor_style` refreshes the window on every call, and
    // this runs on every mouse event of every drag. Without the comparison a
    // slow drag repaints continuously to set the cursor it already has.
    if cx.active_drag_cursor_style() == Some(wanted) {
        return;
    }
    cx.set_active_drag_cursor_style(wanted, window);
}

/// Hand the drag to the compositor, at the moment it begins.
///
/// Deliberately here rather than at the window edge. A drag icon follows the
/// cursor everywhere, this window included, so starting now means one surface
/// carries the chip for the whole gesture and there is no handoff to get wrong.
/// The cost is that a drag grab takes the cursor away from us, which is why
/// refusal is painted on the target rather than shown in the cursor.
pub(super) fn carry_drag(window: &mut Window, cx: &mut App) {
    if !crate::dragging::launch() {
        return;
    }
    // gpui still believes it is running an internal drag, and it will never
    // learn otherwise: the compositor has the pointer, so no mouse-up is coming.
    // Our own data source is the only thing that knows when this ends, and
    // nothing else would ever run to ask it, because a window with the pointer
    // elsewhere receives no input and draws no frames.
    let handle = window.window_handle();
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            if crate::dragging::poll().is_none() {
                continue;
            }
            let _ = cx.update_window(handle, |_, window, cx| {
                cx.stop_active_drag(window);
            });
            break;
        }
    })
    .detach();
}

/// Attach `point_at_drop` for all three payload types.
///
/// Three registrations because `on_drag_move` is generic and fires only for the
/// type it names — the same reason the `drag_over` lines at each drop site come
/// in threes.
///
/// The cursor lives on the drag itself, so nothing here needs undoing when the
/// drag ends: the `AnyDrag` is dropped with it and the next drag starts from
/// its source element's own style again.
pub(super) fn cursor_follows_drops<E: InteractiveElement>(el: E, target: Option<PathBuf>) -> E {
    let (for_paths, for_external, for_members) = (target.clone(), target.clone(), target);
    el.on_drag_move(move |event: &DragMoveEvent<DraggedPaths>, window, cx| {
        point_at_drop(
            event.bounds,
            event.event.position,
            event.dragged_item(),
            for_paths.as_deref(),
            window,
            cx,
        );
    })
    .on_drag_move(move |event: &DragMoveEvent<ExternalPaths>, window, cx| {
        point_at_drop(
            event.bounds,
            event.event.position,
            event.dragged_item(),
            for_external.as_deref(),
            window,
            cx,
        );
    })
    .on_drag_move(move |event: &DragMoveEvent<DraggedMembers>, window, cx| {
        point_at_drop(
            event.bounds,
            event.event.position,
            event.dragged_item(),
            for_members.as_deref(),
            window,
            cx,
        );
    })
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
    pub(super) resolved: std::cell::OnceCell<Vec<PathBuf>>,
    /// Only read before the drag starts; nothing touches it afterwards.
    pub(super) pane: Entity<DirPane>,
    /// The row the drag started on, used when it was not part of the selection.
    pub(super) anchor: PathBuf,
    pub(super) whole_selection: bool,
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
    pub(super) fn resolve(&self, cx: &App) {
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

/// The archive-member counterpart to `DraggedPaths`, for a row with no real
/// path at all.
///
/// A separate type rather than a variant of `DraggedPaths`: that one exists to
/// be promoted into a native drag when it leaves the window
/// (`external_drag_payload`, below), and a member has no file to hand another
/// application before it has been extracted. So this carries archive-relative
/// names instead of paths, is never promoted, and only ever lands on a drop
/// handler inside this app, which extracts on the way in exactly as a paste
/// does.
#[derive(Clone)]
pub struct DraggedMembers {
    /// Filled by `resolve`, same timing as `DraggedPaths` and for the same
    /// reasons.
    pub(super) resolved: std::cell::OnceCell<Vec<String>>,
    pub(super) pane: Entity<DirPane>,
    pub(super) archive: PathBuf,
    pub(super) inside: PathBuf,
    /// The row the drag started on, relative to the archive's root.
    pub(super) anchor: String,
    pub(super) whole_selection: bool,
}

impl DraggedMembers {
    pub(super) fn resolve(&self, cx: &App) {
        self.resolved.get_or_init(|| {
            self.whole_selection
                .then(|| self.pane.read(cx).selected_in_archive())
                .flatten()
                .map(|(_, _, roots)| roots)
                .unwrap_or_else(|| vec![self.anchor.clone()])
        });
    }

    pub(super) fn roots(&self) -> Vec<String> {
        self.resolved
            .get()
            .cloned()
            .unwrap_or_else(|| vec![self.anchor.clone()])
    }
}

/// What the cursor carries during a drag, and the only preview there is.
///
/// A drag that leaves the window keeps working, and carries nothing visible: the
/// icon a Wayland drag can hold has to be handed to `start_drag` from inside
/// gpui, and gpui passes `None`. See the note in `Cargo.toml` for why that is not
/// reachable from out here.
pub(super) struct DragPreview {
    pub(super) label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Once the compositor carries the chip it carries it everywhere, this
        // window included, so drawing here as well would stack two of them.
        if crate::dragging::launched() {
            return div();
        }
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
            // The drag view is drawn at the window's root, outside the
            // workspace's tree, so it does not inherit the family from there.
            .when_some(crate::theming::ui_font(cx), |el, family| {
                el.font_family(family)
            })
            .child(self.label.clone())
    }
}

impl DirPane {
    /// Turn a completed drop into a transfer request.
    ///
    /// `dest` is the folder that was dropped on, which is a row's own path for a
    /// folder row and the pane's directory for the body.
    pub(super) fn accept_drop(
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

        // External paths that are in fact our own, back from a trip outside
        // the window, are an internal drop that lost its papers at the door.
        let source_dir = source_dir
            .map(Path::to_path_buf)
            .or_else(|| crate::dragging::origin_of(&sources));
        let op = match source_dir.as_deref() {
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

    /// Archive members dropped onto a real directory. Always a copy, the same
    /// as `Cut` being refused for a member: nothing here writes to the
    /// archive, so there is no move to offer regardless of what the modifiers
    /// say.
    pub(super) fn accept_extract_drop(
        &mut self,
        archive: PathBuf,
        inside: PathBuf,
        roots: Vec<String>,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if roots.is_empty() {
            return;
        }
        cx.emit(PaneEvent::ExtractDrop {
            archive,
            inside,
            roots,
            dest,
        });
    }

    /// What an entry does under the pointer, the same in every layout: a
    /// click selects, a double-click opens, a right-click opens the menu, a
    /// drag carries it (or the selection) away, and a folder on the disk takes
    /// drops. Applied to whatever element stands for entry `ix`, a row or a
    /// card, after that element is drawn.
    ///
    /// `hover_preview` adds the preview tooltip for a file on the disk.
    pub(super) fn interactive(
        &self,
        row: gpui::Stateful<gpui::Div>,
        ix: usize,
        hover_preview: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let entry = &self.entries[ix];
        let selected = self.selected.contains(ix);
        let is_dir = entry.is_dir;
        // Dropping onto this entry and dragging it away both end in a real
        // file operation, so both hang off this rather than off the key.
        // `None` means a member of an archive, and neither is offered.
        let on_disk = entry.on_disk().map(Path::to_path_buf);
        let renaming = self.renaming.as_ref().is_some_and(|r| r.ix == ix);
        let colors = cx.theme().colors();

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

        // The same drag, offered for a row with no path instead: a member of
        // the archive this pane is browsing. Mutually exclusive with `dragged`
        // above, `on_disk` and `entry.member()` never both answer for one row.
        let dragged_member = entry
            .member()
            .is_some()
            .then(|| {
                let Location::Archive { archive, inside } = &self.dir else {
                    return None;
                };
                let anchor = entry
                    .key()
                    .strip_prefix(archive)
                    .ok()?
                    .to_string_lossy()
                    .into_owned();
                Some(DraggedMembers {
                    resolved: std::cell::OnceCell::new(),
                    pane: cx.entity(),
                    archive: archive.clone(),
                    inside: inside.clone(),
                    anchor,
                    whole_selection: selected,
                })
            })
            .flatten();
        let drag_label: SharedString = if selected {
            match self.selected.len() {
                1 => entry.name.clone().into(),
                n => format!("{n} items").into(),
            }
        } else {
            entry.name.clone().into()
        };
        // What the chip shows, in one place. `DragPreview` draws it inside the
        // window and `dragging` rasterises the same description for the icon
        // that leaves it, so the two cannot drift apart.
        let chip_spec = crate::dragging::ChipSpec {
            label: drag_label.clone(),
            // A folder glyph only for a lone directory; a mixed or multiple
            // selection gets the neutral one.
            folder: is_dir && self.selected.len() <= 1,
            background: colors.elevated_surface_background,
            foreground: colors.text,
            border: colors.border_selected,
            font: crate::theming::ui_font(cx),
        };

        // `dragged` and `dragged_member` are never both `Some` for one row,
        // but both closures below are built regardless, so each needs its own
        // owned copy of the label rather than fighting over the one variable.
        let member_drag_label = drag_label.clone();
        let member_chip_spec = chip_spec.clone();
        // A file on the disk gets a hover preview. Not a folder, not a member
        // of an archive, and not the row being renamed, where the pointer is
        // likely resting on the field rather than asking about the file.
        let preview = on_disk
            .clone()
            .filter(|_| hover_preview && !is_dir && !renaming && crate::preview::enabled(cx))
            .map(|path| crate::preview::Subject {
                path: Arc::from(path),
                modified: entry.modified,
                size: entry.size,
            });

        row
            // A folder is a place to drop things only when it is a folder on
            // the disk. There is nowhere to put a file inside an archive.
            .when_some(on_disk.clone().filter(|_| is_dir), |row, target| {
                let highlight = colors.drop_target_background;
                let refused = cx.theme().status().error_background;
                // A tint either way rather than a border: a row has none, and
                // adding one would resize it as the pointer passed over.
                //
                // `can_drop` is not the gate any more. It used to refuse, which
                // suppressed the highlight and left the cursor to say why, but a
                // compositor drag owns the cursor and we cannot set it. So every
                // drop is taken and the style says whether it will do anything;
                // `accept_drop` already discards an invalid one through
                // `is_valid_drop`, so a refused drop lands on a handler that
                // returns immediately, exactly as it was consumed before.
                cursor_follows_drops(row, Some(target.clone()))
                    .can_drop(|_, _, _| true)
                    .drag_over::<DraggedPaths>({
                        let target = target.clone();
                        move |style, dragged, _, _| {
                            style.bg(tint(drop_allowed(dragged, &target), highlight, refused))
                        }
                    })
                    .drag_over::<ExternalPaths>({
                        let target = target.clone();
                        move |style, dragged, _, _| {
                            style.bg(tint(drop_allowed(dragged, &target), highlight, refused))
                        }
                    })
                    .drag_over::<DraggedMembers>(move |style, _, _, _| style.bg(highlight))
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
                    .on_drop(cx.listener({
                        let target = target.clone();
                        move |this, paths: &ExternalPaths, window, cx| {
                            this.accept_drop(
                                paths.paths().to_vec(),
                                None,
                                target.clone(),
                                window,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let target = target.clone();
                        move |this, dragged: &DraggedMembers, _window, cx| {
                            this.accept_extract_drop(
                                dragged.archive.clone(),
                                dragged.inside.clone(),
                                dragged.roots(),
                                target.clone(),
                                cx,
                            );
                        }
                    }))
            })
            .when_some(dragged, |row, dragged| {
                // Also where the payload settles what it carries: see `resolve`.
                let spec = chip_spec.clone();
                let row = row.on_drag(dragged, move |dragged: &DraggedPaths, _, window, cx| {
                    dragged.resolve(cx);
                    // The label and the theme are only reachable from the row
                    // that started the drag, so the chip is built here and the
                    // compositor takes it immediately.
                    crate::dragging::arm(&spec, &dragged.paths(), &dragged.source_dir);
                    carry_drag(window, cx);
                    cx.new(|_| DragPreview {
                        label: drag_label.clone(),
                    })
                });
                // Only when hoja is not carrying the drag itself. Both would
                // start a gesture and the compositor would take whichever asked
                // first, so this is either/or rather than belt and braces.
                row.when(!crate::dragging::available(), |row| {
                    // The resolver runs once, at promotion, never per frame,
                    // which is why the is_dir probes belong here and not in the
                    // payload.
                    row.external_drag_payload(|dragged: &DraggedPaths, _, _: &mut App| {
                        Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
                            dragged
                                .paths()
                                .into_iter()
                                .map(|path| (path.is_dir(), path))
                                .map(|(is_dir, path)| (path, is_dir)),
                        )))
                    })
                })
            })
            .when_some(dragged_member, |row, dragged| {
                // Carried out of the window like any other drag, with nothing
                // extracted up front: the files come into being only when a
                // target has taken the drop and asks where they are. A drop
                // back inside hoja never reaches that point and goes through
                // `accept_extract_drop` as it always did.
                let spec = member_chip_spec.clone();
                row.on_drag(dragged, move |dragged: &DraggedMembers, _, window, cx| {
                    dragged.resolve(cx);
                    crate::dragging::arm_extracting(
                        &spec,
                        dragged.archive.clone(),
                        dragged.inside.clone(),
                        dragged.roots(),
                    );
                    carry_drag(window, cx);
                    cx.new(|_| DragPreview {
                        label: member_drag_label.clone(),
                    })
                })
            })
            .when_some(preview, |row, subject| {
                row.tooltip(crate::preview::tooltip(subject))
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
}
