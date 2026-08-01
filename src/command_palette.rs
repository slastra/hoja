//! Command palette: `ctrl-shift-p`, fuzzy match, run.
//!
//! Follows Zed's conventions, because the project lives in that lineage and
//! because they are right. Zed's palette is three crates deep
//! (`command_palette` → `picker` → `ui`); we depend on none of them, since
//! `picker` is a general delegate-driven component with resizable geometry,
//! preview panes, multi-select, and sqlite persistence, and pane has exactly
//! one picker.
//!
//! `humanize_action_name` and `normalize_action_query` below are copied
//! verbatim from `command_palette`, with their tests. They are pure
//! `&str -> String` and the acronym handling took real work to get right.
//!
//! Matching is synchronous: Zed matches ~2000 actions on a background executor,
//! pane has a few dozen, so the whole pending-update and cancellation machinery
//! in `picker` disappears. What we do keep is the smart-case rule, which Zed
//! documented the hard way: match case-insensitively and use case only as a
//! scoring signal, because matching case-sensitively rejects
//! "Pane: Split Right" against the action named `pane: split right`, which is
//! the palette's whole use case.

use std::collections::HashMap;

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use theme::ActiveTheme;

use crate::path_editor::{PathEditor, PathEditorEvent};
use crate::picker::{highlighted_label, match_entries};

actions!(palette, [Toggle]);

// The palette owns its navigation keys so menu bindings cannot leak in, and so
// changing menu keys cannot silently change palette behaviour.
actions!(palette, [SelectNext, SelectPrevious]);

const ROW_HEIGHT: f32 = 28.;
/// Ten rows. Beyond this the list scrolls rather than growing, so the palette
/// never runs off the bottom of the window.
const MAX_VISIBLE_ROWS: f32 = 10.;

/// Actions that exist to be bound to a key, not to be read from a list. Row
/// movement and selection are meaningless as palette entries: you cannot
/// usefully "run" `move down` from a modal that closes when you do.
const HIDDEN: &[&str] = &[
    "pane::MoveUp",
    "pane::MoveDown",
    "pane::MovePageUp",
    "pane::MovePageDown",
    "pane::MoveToTop",
    "pane::MoveToBottom",
    "pane::ExtendUp",
    "pane::ExtendDown",
    "pane::ExtendPageUp",
    "pane::ExtendPageDown",
    "pane::ExtendToTop",
    "pane::ExtendToBottom",
    "pane::CursorUp",
    "pane::CursorDown",
    "pane::ToggleSelection",
    "palette::Toggle",
];

/// How many launches back the ordering remembers.
const HISTORY_LIMIT: usize = 200;

struct Command {
    /// Humanized, e.g. `pane: split right`.
    name: String,
    /// The action's registered name, used as the history key.
    id: String,
    action: Box<dyn Action>,
    /// Rendered as keycaps; empty when the action has no binding.
    keystrokes: Vec<String>,
}

pub struct CommandPalette {
    focus_handle: FocusHandle,
    query: Entity<PathEditor>,
    commands: Vec<Command>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<StringMatch>,
    selected: usize,
    scroll: UniformListScrollHandle,
    /// Where focus came from. Load-bearing three times over — see `new`.
    previous_focus: FocusHandle,
    history: HashMap<String, usize>,
    _subscriptions: Vec<Subscription>,
}

pub fn humanize_action_name(name: &str) -> String {
    let chars = name.chars().collect::<Vec<_>>();
    let capacity = name.len() + chars.iter().filter(|c| c.is_uppercase()).count();
    let mut result = String::with_capacity(capacity);
    let mut index = 0;

    while index < chars.len() {
        let char = chars[index];
        if char == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
            index += 1;
        } else if char == '_' {
            result.push(' ');
            index += 1;
        } else if char.is_uppercase() {
            let start = index;
            index += 1;
            while chars
                .get(index)
                .is_some_and(|next_char| next_char.is_uppercase())
            {
                index += 1;
            }

            let uppercase_run = &chars[start..index];
            if uppercase_run.len() > 1 {
                let split_before_last = chars
                    .get(index)
                    .is_some_and(|next_char| next_char.is_lowercase());
                let acronym_end = if split_before_last {
                    uppercase_run.len() - 1
                } else {
                    uppercase_run.len()
                };

                if acronym_end > 0 {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(&uppercase_run[..acronym_end]);
                }

                if split_before_last {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(uppercase_run[acronym_end].to_lowercase());
                }
            } else {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                result.extend(char.to_lowercase());
            }
        } else {
            result.push(char);
            index += 1;
        }
    }

    result
}

pub fn normalize_action_query(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_char = None;

    for char in input.trim().chars() {
        let normalized_char = if char == '_' { ' ' } else { char };
        match (last_char, normalized_char) {
            (Some(':'), ':') => continue,
            (Some(last_char), c) if last_char.is_whitespace() && c.is_whitespace() => {
                continue;
            }
            _ => {
                last_char = Some(normalized_char);
            }
        }
        result.push(normalized_char);
    }

    result
}

impl CommandPalette {
    /// `previous_focus` must be captured by the caller *before* the palette
    /// takes focus. It is load-bearing three separate times:
    ///
    /// - `available_actions` walks the dispatch path from the focused element
    ///   upward, so calling it after the palette focuses enumerates the
    ///   palette's own actions instead of the pane's.
    /// - Bindings are resolved against that handle, so the palette shows the
    ///   key that would actually fire in the pane you came from.
    /// - On confirm, focus must return there *before* dispatch, or the action
    ///   dispatches into the palette's own path where nothing handles it.
    pub fn new(previous_focus: FocusHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let query = cx.new(|cx| PathEditor::new(String::new(), window, cx).with_placeholder("Run a command"));

        // The query field carries the AddressBar context *inside* the palette's
        // own, and the deeper context wins, so enter and escape reach the
        // editor rather than the palette's bindings. Its events are the right
        // signal anyway: commit means run, cancel means close.
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                PathEditorEvent::Edited => this.rematch(cx),
                PathEditorEvent::Committed(_) => this.confirm(window, cx),
                PathEditorEvent::Cancelled => cx.emit(DismissEvent),
            }),
        ];

        let history = load_history();
        let mut commands = available_commands(&previous_focus, window, cx);
        // Sorting by use *before* building candidates makes the candidate id
        // the recency rank. The matcher already breaks score ties on ascending
        // id, so frequently used commands win ties for free, and an empty query
        // shows most-used-first.
        commands.sort_by(|a, b| {
            history
                .get(&b.id)
                .unwrap_or(&0)
                .cmp(history.get(&a.id).unwrap_or(&0))
                .then_with(|| a.name.cmp(&b.name))
        });
        let candidates: Vec<StringMatchCandidate> = commands
            .iter()
            .enumerate()
            .map(|(ix, command)| StringMatchCandidate::new(ix, &command.name))
            .collect();

        let mut palette = Self {
            focus_handle,
            query,
            commands,
            candidates,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            previous_focus,
            history,
            _subscriptions: subscriptions,
        };
        palette.rematch(cx);
        palette
    }

    pub fn query_focus(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }

    fn rematch(&mut self, cx: &mut Context<Self>) {
        let raw = self.query.read(cx).text().to_string();
        let query = normalize_action_query(&raw);

        // An empty query keeps recency order, which is why the candidates were
        // built in that order.
        self.matches = match_entries(&self.candidates, &query);
        self.selected = 0;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len() as isize;
        // Wrapping: a palette is a short list and reaching the end by holding
        // down should come back around rather than stick.
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self
            .matches
            .get(self.selected)
            .and_then(|m| self.commands.get(m.candidate_id))
        else {
            return;
        };
        let action = command.action.boxed_clone();
        let id = command.id.clone();

        *self.history.entry(id).or_insert(0) += 1;
        save_history(&self.history, cx);

        // Order matters: focus first, then dismiss, then dispatch. Dispatching
        // while the palette still holds focus sends the action into the
        // palette's dispatch path, where no listener handles it.
        window.focus(&self.previous_focus, cx);
        cx.emit(DismissEvent);
        window.dispatch_action(action, cx);
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

/// Actions reachable from `origin`, humanized and paired with their binding.
///
/// `available_actions` is deliberately not `all_action_names`: it returns only
/// what some element on the current dispatch path actually handles, so with no
/// selection `rename` does not appear. That is what makes the list honest.
///
/// An action that cannot be built from its type is dropped by gpui before we
/// see it; if a command goes missing from the palette, that is the reason.
fn available_commands(
    origin: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Vec<Command> {
    window
        .available_actions(cx)
        .into_iter()
        .filter(|action| !HIDDEN.contains(&action.name()))
        .map(|action| {
            let keystrokes = window
                .highest_precedence_binding_for_action_in(action.as_ref(), origin)
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| keystroke.to_string())
                        .collect()
                })
                .unwrap_or_default();
            Command {
                name: humanize_action_name(action.name()),
                id: action.name().to_string(),
                action,
                keystrokes,
            }
        })
        .collect()
}

fn history_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))?;
    Some(base.join("pane/command-history.json"))
}

/// Launch counts by action name. In-memory only would reset every start, which
/// makes recency ordering close to pointless.
fn load_history() -> HashMap<String, usize> {
    let Some(path) = history_path() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json_lenient::from_str(&text).unwrap_or_default()
}

fn save_history(history: &HashMap<String, usize>, cx: &App) {
    let Some(path) = history_path() else {
        return;
    };
    // Keep only the most used, so the file cannot grow without bound.
    let mut entries: Vec<(String, usize)> =
        history.iter().map(|(k, v)| (k.clone(), *v)).collect();
    entries.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    entries.truncate(HISTORY_LIMIT);
    let trimmed: HashMap<String, usize> = entries.into_iter().collect();

    cx.background_spawn(async move {
        let Ok(text) = serde_json_lenient::to_string(&trimmed) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    })
    .detach();
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let count = self.matches.len();

        let list = uniform_list(
            "commands",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|ix| this.render_row(ix, window, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        // uniform_list virtualizes, so it needs a definite height rather than a
        // maximum: given only a max it renders nothing. Size it to the rows
        // there are, capped, so a three-result palette is three rows tall.
        .h(px(ROW_HEIGHT * (count as f32).min(MAX_VISIBLE_ROWS)));

        div()
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("palette")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
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
                // Keep the modal from collapsing and jumping when nothing
                // matches.
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(colors.text_muted)
                        .child("No matching commands"),
                )
            })
            .when(count > 0, |el| el.child(list))
    }
}

impl CommandPalette {
    fn render_row(
        &self,
        ix: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let Some(matched) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(command) = self.commands.get(matched.candidate_id) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected;

        let keycaps: Vec<_> = command
            .keystrokes
            .iter()
            .map(|key| {
                div()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.element_background)
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(key.clone())
            })
            .collect();

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
            .text_color(colors.text)
            .when(selected, |row| row.bg(colors.element_selected))
            // Hovering selects, which is a large part of why Zed's palette
            // feels responsive rather than modal.
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
                    .flex_1()
                    .truncate()
                    .child(highlighted_label(command.name.clone(), matched, window, cx)),
            )
            .children(keycaps)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copied with the functions they cover.
    #[test]
    fn humanizing_action_names() {
        assert_eq!(
            humanize_action_name("pane::SplitRight"),
            "pane: split right"
        );
        assert_eq!(humanize_action_name("pane::Delete"), "pane: delete");
        assert_eq!(
            humanize_action_name("dir_pane::ToggleHiddenFiles"),
            "dir pane: toggle hidden files"
        );
        // Acronyms survive, including one followed by a word.
        assert_eq!(humanize_action_name("editor::OpenURL"), "editor: open URL");
        assert_eq!(
            humanize_action_name("editor::OpenURLParser"),
            "editor: open URL parser"
        );
    }

    #[test]
    fn normalizing_a_typed_query() {
        assert_eq!(
            normalize_action_query("pane: split right"),
            "pane: split right"
        );
        assert_eq!(
            normalize_action_query("pane:  split  right"),
            "pane: split right"
        );
        // A keymap-style query still matches the humanized name.
        assert_eq!(normalize_action_query("pane::split"), "pane:split");
        assert_eq!(normalize_action_query("toggle_hidden"), "toggle hidden");
    }
}
