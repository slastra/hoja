//! The list layout: the column header, its resize handles, and the rows.
//! The colours and icon an entry is drawn with are here too; the grid's
//! cards use them as well.

use super::*;

/// The bar that stands in for a folder size still being counted, and the breath
/// it takes. Narrower than the figure it precedes, and faint at both ends of the
/// cycle: it is the answer to "is anything happening", not a thing to read.
pub(super) const COUNTING_BAR_WIDTH: f32 = 38.;

pub(super) const COUNTING_BAR_HEIGHT: f32 = 4.;

pub(super) const COUNTING_PERIOD: Duration = Duration::from_millis(1600);

pub(super) const COUNTING_ALPHA_LOW: f32 = 0.10;

pub(super) const COUNTING_ALPHA_HIGH: f32 = 0.32;

/// The bar a folder's size shows while it is counted, in a row or on a card.
pub(super) fn counting_bar(color: gpui::Hsla, id: gpui::ElementId) -> impl IntoElement + use<> {
    div()
        .w(px(COUNTING_BAR_WIDTH))
        .h(px(COUNTING_BAR_HEIGHT))
        .rounded_full()
        .bg(color)
        .with_animation(
            id,
            gpui::Animation::new(COUNTING_PERIOD)
                .repeat()
                .with_easing(gpui::pulsating_between(
                    COUNTING_ALPHA_LOW,
                    COUNTING_ALPHA_HIGH,
                )),
            |bar, delta| bar.opacity(delta),
        )
}

impl DirPane {
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
    pub(super) fn render_column_handle(
        &self,
        column: Column,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
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

    pub(super) fn render_header(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
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
        let widths = self.widths;
        let moving = self.moving_column();
        let carried_bg = colors.element_selected;
        // `None` is the Name column, which is not in the table: it flexes to
        // fill, it holds the icon and the rename editor, and it does not move.
        // Reordering permutes the fixed columns to its right.
        let head = |column: Option<Column>| {
            let (key, label, width, right) = match column {
                Some(c) => (
                    c.sort_key(),
                    c.label(),
                    Some(widths.get(c)),
                    c.aligns_right(),
                ),
                None => (SortKey::Name, "Name", None, false),
            };
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
                // The header being carried, marked more strongly than its
                // values are: this is the part with a hand on it, and the
                // column below it follows.
                .when(column.is_some() && column == moving, |el| el.bg(carried_bg))
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
                .when_some(column, |el, column| {
                    el.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, _| {
                            this.moving = Some(ColumnMoveDrag {
                                column,
                                start_x: event.position.x,
                                start_slot: this.view.columns.slot_of(column).unwrap_or(0),
                                start_pitches: this
                                    .view
                                    .columns
                                    .iter()
                                    .map(|c| f32::from(this.widths.get(c)) + COL_HANDLE_WIDTH)
                                    .collect(),
                                start_layout: this.view.columns,
                                dragged: false,
                                moved_by: 0.,
                            });
                        }),
                    )
                    .on_drag(
                        ColumnMove {
                            pane: cx.entity_id(),
                            column,
                        },
                        |_, _, _, cx| cx.new(|_| EmptyDrag),
                    )
                    // The click this header would have had.
                    //
                    // gpui starts a drag two pixels from the mouse down
                    // (`DRAG_THRESHOLD`) and drops the pending click the moment
                    // it does, so a header click with any hand movement in it
                    // sorts nothing. That is the right trade for a listing row,
                    // where the drag is the intent; it is the wrong one here,
                    // where the click is. A drag that never moved a column was
                    // a click, and this is where that is decided: `on_click`
                    // has already been suppressed in exactly this case, so the
                    // two can never both fire.
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                            // How far the pointer went, not whether the layout
                            // ended where it started. Those are different
                            // questions: a drag that goes out past two columns
                            // and comes back lands on the slot it began in, and
                            // reading that as a click turns a cancelled reorder
                            // into a silent re-sort.
                            let wandered = this
                                .moving
                                .as_ref()
                                .filter(|drag| drag.column == column && drag.dragged)
                                .map(|drag| drag.moved_by.abs());
                            if wandered.is_none_or(|far| far > HEADER_CLICK_SLOP) {
                                return;
                            }
                            this.moving = None;
                            this.toggle_sort(key, cx);
                        }),
                    )
                })
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            // No surface of its own: the pane's background, between the
            // toolbar's rule above and its own below, as Zed sets off a
            // panel's header.
            .border_b_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(content)
            // Released, wherever that happened. Two handlers because
            // `on_mouse_up` is scoped to this element and a drag very often
            // ends somewhere else entirely; without the `_out` half the tint
            // outlives the gesture that put it there. Both run after the
            // headers' own mouse-up handlers, which bubble outwards from the
            // cell, so the click this recovers is decided before the state it
            // is decided from goes away.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.finish_move(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.finish_move(cx);
                }),
            )
            // One registration for all six, rather than one per header as the
            // resize needs: that one has to know which handle it belongs to,
            // and this one reads the column off the payload.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ColumnMove>, _window, cx| {
                    let active = *event.drag(cx);
                    if active.pane != cx.entity_id() {
                        return;
                    }
                    let Some(layout) = this
                        .moving
                        .as_mut()
                        .filter(|drag| drag.column == active.column)
                        .map(|drag| {
                            drag.dragged = true;
                            let dx = f32::from(event.event.position.x - drag.start_x);
                            drag.moved_by = dx;
                            let slot = target_slot(&drag.start_pitches, drag.start_slot, dx);
                            drag.start_layout.moved(drag.column, slot)
                        })
                    else {
                        return;
                    };
                    if this.view.columns == layout {
                        return;
                    }
                    this.view.columns = layout;
                    cx.emit(PaneEvent::ViewChanged);
                    cx.notify();
                }),
            )
            .child(head(None));

        self.view.columns.iter().fold(header, |header, column| {
            header
                .child(self.render_column_handle(column, cx))
                .child(head(Some(column)))
        })
    }

    /// `use<>` opts out of capturing the `&self` / `&Context` lifetimes: the returned
    /// element owns every value it needs, and `uniform_list`'s callback must return
    /// something with no borrows outstanding.
    pub(super) fn render_entry(
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
        let (content, name_color) = self.entry_colors(entry, cx);
        let icon_path = self.entry_icon(entry, cx);

        // Cells line up with the header by using the same widths and the same
        // handle-sized spacers where the dividers sit.
        let spacer = || div().w(px(COL_HANDLE_WIDTH)).flex_none();
        let carried_bg = colors.ghost_element_selected;
        let cell = move |cell: Cell| {
            let Cell {
                width,
                text,
                numeric,
                counting,
                right,
                carried,
            } = cell;
            let body = div()
                .w(width)
                .flex_none()
                .px_2()
                .truncate()
                .text_color(content)
                // Translucent, so a selected row still reads through it: the
                // column is being moved, the row underneath is not, and both
                // facts are worth keeping on screen at once.
                .when(carried, |el| el.bg(carried_bg))
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
                .child(counting_bar(content, ("counting", ix).into()))
                .into_any_element()
        };

        // Resolved before the element is built: `use<>` forbids capturing `self`,
        // and this keeps the cells in lockstep with the header.
        // Resolved here with everything else the row needs, so nothing borrows
        // `self` past the end of this function.
        let folder_bytes = self.folder_bytes(ix);
        let moving = self.moving_column();
        let numeric = crate::theming::numeric_font(cx);
        let counting = self.counting_size(ix);
        let cells: Vec<Cell> = self
            .view
            .columns
            .iter()
            .map(|column| Cell {
                width: self.widths.get(column),
                text: column.value(entry, folder_bytes, now),
                numeric: column.is_numeric().then(|| numeric.clone()).flatten(),
                // Only the column that holds a folder size has anything to wait for.
                counting: counting && column == Column::Size,
                right: column.aligns_right(),
                carried: moving == Some(column),
            })
            .collect();

        // A search result is labelled by where it sits; everything else by its
        // own name.
        let label = entry.relative.clone().unwrap_or_else(|| entry.name.clone());

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
            // The lead row, the one clicked or moved to, where ctrl-arrow goes
            // from and what ctrl-space acts on, outlined in the accent colour
            // while this pane has the keys, as Zed outlines the active entry
            // of a focused panel. Every row carries the border, transparent
            // on the rest: `uniform_list` measures one row, so the geometry
            // cannot differ between them.
            .border_1()
            .border_color(if is_lead && self.active {
                // The colour Zed's project panel outlines its active entry
                // in. Some themes make `border_focused` a dark surface tone.
                colors.panel_focused_border
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

        let row = cells
            .into_iter()
            .fold(row, |row, spec| row.child(spacer()).child(cell(spec)));
        self.interactive(row, ix, true, cx)
    }

    /// The colors an entry is drawn in, as (content, name): everything but the
    /// name in the first, the name in the second.
    ///
    /// One content colour throughout: secondary columns and icons. No accent
    /// on directories, no muting on metadata: hierarchy comes from position
    /// and the selection background instead of from hue.
    ///
    /// Git status is the one exception, and only on the name. Hue here
    /// carries information rather than decoration, which is the whole test: a
    /// green name means untracked, and nothing else says so. Both quiet down
    /// together when this is not the pane the keys are acting on.
    pub(super) fn entry_colors(&self, entry: &DirEntry, cx: &App) -> (gpui::Hsla, gpui::Hsla) {
        let base = cx.theme().colors().text;
        // The theme's status colours, which are what Zed's project panel uses
        // for a name (`Color::Created` and the rest), rather than its
        // `version_control_*` colours, which are tuned for a diff gutter.
        let status = cx.theme().status();
        let name_base = self
            .git
            .get(&entry.name)
            .map(|git| match git {
                GitStatus::Added => status.created,
                GitStatus::Modified => status.modified,
                GitStatus::Deleted => status.deleted,
                GitStatus::Renamed => status.renamed,
                GitStatus::Conflict => status.conflict,
                GitStatus::Ignored => status.ignored,
                GitStatus::Unmodified => base,
            })
            .unwrap_or(base);
        (quieted(base, self.active), quieted(name_base, self.active))
    }

    /// The icon for an entry. Resolution order handles the messy cases: full
    /// filename against stems and suffixes (`eslint.config.js`), repeated
    /// `split_once('.')` (`auth.module.js`), multiple extensions
    /// (`Component.stories.tsx`), hidden files (`.gitignore`), bare
    /// extension, then the `"default"` key.
    pub(super) fn entry_icon(&self, entry: &DirEntry, cx: &App) -> Option<SharedString> {
        if entry.is_dir {
            FileIcons::get_folder_icon(false, entry.key(), cx)
        } else {
            FileIcons::get_icon(entry.key(), cx)
        }
    }
}
