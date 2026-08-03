//! `ctrl-p`: jump to home, a bookmark, or an attached volume.
//!
//! The command palette's sibling. It shares matching and label highlighting
//! through `picker`, and differs in the two ways that matter: entries are
//! places rather than actions, and choosing one navigates the active pane
//! instead of dispatching an action. An unmounted volume is mounted first.
//!
//! `ctrl-e` on a mounted, removable place ejects it instead of navigating:
//! the symmetric other half of choosing an unmounted one to mount it. Scoped
//! to the shared `picker` key context rather than a context of its own, which
//! means it is also, technically, bindable while the command palette has
//! focus; nothing there registers a handler for it, so it is simply inert.

use fuzzy_nucleo::StringMatchCandidate;
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window,
    actions, div, prelude::*, uniform_list,
};
use std::path::PathBuf;
use theme::ActiveTheme;

use crate::path_editor::{PathEditor, PathEditorEvent};
use crate::picker::{self, PickerState, SelectNext, SelectPrevious, highlighted_label};
use crate::places::{self, Place};

actions!(places, [Toggle, Eject]);

const ROW_HEIGHT: f32 = 32.;

/// Match the detail as well as the label, so typing part of a path finds a
/// bookmark whose name does not contain it.
fn candidate((ix, place): (usize, &Place)) -> StringMatchCandidate {
    StringMatchCandidate::new(ix, &format!("{} {}", place.label(), place.detail())[..])
}

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
    /// Unmount this, which is already mounted at `mount`.
    ///
    /// Same reasoning as `Mount`: the finder dismisses (and drops) the moment
    /// an action is chosen, so the workspace has to be the one to actually do
    /// it and report back. `mount` travels along so the workspace can bounce
    /// any pane that was sitting under it once the unmount succeeds, rather
    /// than leaving a pane pointed at a directory that is no longer there.
    Unmount {
        device: PathBuf,
        label: String,
        mount: PathBuf,
    },
}

pub struct PlaceFinder {
    focus_handle: FocusHandle,
    picker: PickerState,
    places: Vec<Place>,
    _subscriptions: Vec<Subscription>,
}

impl PlaceFinder {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let query = cx.new(|cx| {
            PathEditor::new(String::new(), window, cx)
                .with_placeholder("Go to a place")
                .bare()
        });

        // As in the palette: the query field carries the AddressBar context
        // inside this one, and the deeper context wins, so enter and escape
        // reach the editor. Its events mean exactly open and close.
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                PathEditorEvent::Edited => this.rematch(cx),
                PathEditorEvent::Committed(_) => this.confirm(window, cx),
                PathEditorEvent::Cancelled => cx.emit(DismissEvent),
            }),
        ];

        // Home and bookmarks are a file read; volumes shell out to lsblk, which
        // stalls on a spun-down or flaky enclosure. Open on what is free and
        // splice the drives in when they arrive, the shape `resolve_open_with`
        // uses for the context menu.
        let places = places::local();
        let mut picker = PickerState::new(query);
        picker.candidates = places.iter().enumerate().map(candidate).collect();

        let mut finder = Self {
            focus_handle,
            picker,
            places,
            _subscriptions: subscriptions,
        };
        finder.rematch(cx);
        finder.load_volumes(cx);
        finder
    }

    fn load_volumes(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let volumes = cx.background_spawn(async { places::volumes() }).await;
            if volumes.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let base = this.places.len();
                this.picker.candidates.extend(
                    volumes
                        .iter()
                        .enumerate()
                        .map(|(ix, place)| candidate((base + ix, place))),
                );
                this.places.extend(volumes);
                this.rematch_keeping_selection(cx);
            });
        })
        .detach();
    }

    pub fn query_focus(&self, cx: &App) -> FocusHandle {
        self.picker.query.focus_handle(cx)
    }

    fn rematch(&mut self, cx: &mut Context<Self>) {
        let query = self.picker.query.read(cx).text().to_string();
        self.picker.rematch(query.trim());
        cx.notify();
    }

    /// The same, for the volumes arriving late: see `rematch_keeping_selection`.
    fn rematch_keeping_selection(&mut self, cx: &mut Context<Self>) {
        let query = self.picker.query.read(cx).text().to_string();
        self.picker.rematch_keeping_selection(query.trim());
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.picker.move_selection(1);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.picker.move_selection(-1);
        cx.notify();
    }

    fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((place, _)) = self.picker.chosen(&self.places) else {
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

    /// Unmount whatever is highlighted, if it is something that can be.
    ///
    /// Silently does nothing on a row that is not a mounted, removable
    /// device: home, a bookmark, and an already-unmounted volume all answer
    /// `removable()` with `None`, and there is no action to take on any of
    /// them here.
    fn eject(&mut self, _: &Eject, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((place, _)) = self.picker.chosen(&self.places) else {
            return;
        };
        let Place::Dir {
            path,
            label,
            device: Some(device),
            ..
        } = place
        else {
            return;
        };
        cx.emit(PlaceEvent::Unmount {
            device: device.clone(),
            label: label.clone(),
            mount: path.clone(),
        });
        cx.emit(DismissEvent);
    }

    fn render_row(&self, ix: usize, window: &Window, cx: &Context<Self>) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let Some(matched) = self.picker.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(place) = self.places.get(matched.candidate_id) else {
            return div().into_any_element();
        };

        picker::row(ix, ROW_HEIGHT, ix == self.picker.selected, cx)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered && this.picker.selected != ix {
                    this.picker.selected = ix;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.picker.selected = ix;
                this.confirm(window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .text_color(colors.text)
                    // The candidate is "label detail", so the label's own
                    // highlight offsets are the leading part of the match.
                    .child(highlighted_label(
                        place.label().to_string(),
                        matched,
                        window,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(place.detail().to_string()),
            )
            .when(place.removable().is_some(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child("ctrl-e eject"),
                )
            })
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
        let list = uniform_list(
            "places",
            self.picker.matches.len(),
            cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|ix| this.render_row(ix, window, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.picker.scroll)
        .h(picker::list_height(&self.picker, ROW_HEIGHT))
        .into_any_element();

        picker::shell(&self.picker, "No matching places", list, cx)
            .track_focus(&self.focus_handle)
            .key_context(picker::KEY_CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::eject))
    }
}
