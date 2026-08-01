//! `ctrl-p`: jump to home, a bookmark, or an attached volume.
//!
//! The command palette's sibling. It shares matching and label highlighting
//! through `picker`, and differs in the two ways that matter: entries are
//! places rather than actions, and choosing one navigates the active pane
//! instead of dispatching an action. An unmounted volume is mounted first.

use fuzzy_nucleo::{StringMatchCandidate, StringMatch};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use std::path::PathBuf;
use theme::ActiveTheme;

use crate::path_editor::{PathEditor, PathEditorEvent};
use crate::picker::{highlighted_label, match_entries};
use crate::places::{self, Place};

actions!(places, [Toggle]);

const ROW_HEIGHT: f32 = 32.;
const MAX_VISIBLE_ROWS: f32 = 10.;

/// Emitted upward; the workspace owns the active pane and the status strip.
pub enum PlaceEvent {
    /// Point the active pane here.
    Open(PathBuf),
    /// Mount this volume, then go there.
    ///
    /// The finder does not mount it itself: it dismisses as soon as a place is
    /// chosen, and a dismissed entity is dropped, so anything waiting on a
    /// background task through a handle to it never reports back. The mount
    /// succeeded and the navigation was thrown away. The workspace outlives the
    /// modal, so the work belongs there.
    Mount { device: PathBuf, label: String },
}

pub struct PlaceFinder {
    focus_handle: FocusHandle,
    query: Entity<PathEditor>,
    places: Vec<Place>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl PlaceFinder {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let query = cx.new(|cx| {
            PathEditor::new(String::new(), window, cx).with_placeholder("Go to a place")
        });

        // As in the palette: the query field carries the AddressBar context
        // inside this one, and the deeper context wins, so enter and escape
        // reach the editor. Its events mean exactly open and close.
        let mut subscriptions = vec![cx.subscribe_in(
            &query,
            window,
            |this, _, event, window, cx| match event {
                PathEditorEvent::Edited => this.rematch(cx),
                PathEditorEvent::Committed(_) => this.confirm(window, cx),
                PathEditorEvent::Cancelled => cx.emit(DismissEvent),
            },
        )];
        subscriptions.push(cx.on_blur(&focus_handle, window, |_, _, cx| cx.emit(DismissEvent)));

        let places = places::all();
        let candidates = places
            .iter()
            .enumerate()
            .map(|(ix, place)| {
                // Match the detail as well as the label, so typing part of a
                // path finds a bookmark whose name does not contain it.
                StringMatchCandidate::new(ix, &format!("{} {}", place.label(), place.detail())[..])
            })
            .collect();

        let mut finder = Self {
            focus_handle,
            query,
            places,
            candidates,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        finder.rematch(cx);
        finder
    }

    pub fn query_focus(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }

    fn rematch(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text().to_string();
        self.matches = match_entries(&self.candidates, query.trim());
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(place) = self
            .matches
            .get(self.selected)
            .and_then(|m| self.places.get(m.candidate_id))
        else {
            return;
        };

        match place.clone() {
            Place::Dir { path, .. } => {
                cx.emit(PlaceEvent::Open(path));
                cx.emit(DismissEvent);
            }
            Place::Volume { device, label, .. } => {
                cx.emit(PlaceEvent::Mount { device, label });
                cx.emit(DismissEvent);
            }
        }
    }

    fn render_row(&self, ix: usize, window: &Window, cx: &Context<Self>) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let Some(matched) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(place) = self.places.get(matched.candidate_id) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected;

        div()
            .id(ix)
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_sm()
            .when(selected, |row| row.bg(colors.element_selected))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered && this.selected != ix {
                    this.selected = ix;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = ix;
                this.confirm(window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .text_color(colors.text)
                    // The candidate is "label detail", so the label's own
                    // highlight offsets are the leading part of the match.
                    .child(highlighted_label(place.label().to_string(), matched, window, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(place.detail().to_string()),
            )
            .into_any_element()
    }
}

impl Focusable for PlaceFinder {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for PlaceFinder {}
impl EventEmitter<PlaceEvent> for PlaceFinder {}

impl Render for PlaceFinder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let count = self.matches.len();

        div()
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("palette")
            .on_action(cx.listener(|this, _: &crate::command_palette::SelectNext, _, cx| {
                this.move_selection(1, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::command_palette::SelectPrevious, _, cx| {
                this.move_selection(-1, cx)
            }))
            .on_action(cx.listener(|_this, _: &crate::command_palette::Cancel, _, cx| {
                cx.emit(DismissEvent)
            }))
            .flex()
            .flex_col()
            .w(px(544.))
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .text_sm()
                    .child(self.query.clone()),
            )
            .when(count == 0, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(colors.text_muted)
                        .child("No matching places"),
                )
            })
            .when(count > 0, |el| {
                el.child(
                    uniform_list(
                        "places",
                        count,
                        cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                            range
                                .map(|ix| this.render_row(ix, window, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.scroll)
                    // Definite, not a maximum: uniform_list virtualizes and
                    // renders nothing when given only a max.
                    .h(px(ROW_HEIGHT * (count as f32).min(MAX_VISIBLE_ROWS))),
                )
            })
    }
}
