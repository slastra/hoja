//! Single-line path input, adapted from gpui's `examples/input.rs` (Apache-2.0).
//!
//! Three deliberate departures from the example:
//! - **Horizontal scroll** (`scroll_x`): the example paints long content past the
//!   field's right edge; paths are long, so the caret is kept visible and the
//!   line is clipped with a content mask.
//! - **IME/marked-text handling is kept in full.** On Wayland, xkb compose
//!   sequences and dead keys arrive via `SetMarkedText` even with no input
//!   method running; stubbing it duplicates preedit text.
//! - The example's `character_index_for_point` is dropped (it is buggy and never
//!   called by the Linux backends).

use std::ops::Range;

use gpui::{
    App, Bounds, Context, DismissEvent, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window, actions, div, fill, point,
    prelude::*, px, relative, size,
};
use theme::ActiveTheme;
use unicode_segmentation::UnicodeSegmentation;

actions!(
    address_bar,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        SelectToHome,
        SelectToEnd,
        DeleteWordLeft,
        DeleteWordRight,
        Copy,
        Cut,
        Paste,
        Confirm,
        CancelEdit,
    ]
);

/// A "word" character for word-wise movement: alphanumeric or underscore.
/// Runs of anything else (path separators, dots, dashes) are boundaries,
/// matching GTK-entry conventions.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// End of the current or next word, GTK style: skip separators, then the word.
fn next_word_boundary(text: &str, offset: usize) -> usize {
    let mut chars = text[offset..].char_indices().peekable();
    let mut pos = offset;
    // Skip the separator run.
    while let Some(&(ix, c)) = chars.peek() {
        if is_word_char(c) {
            break;
        }
        pos = offset + ix + c.len_utf8();
        chars.next();
    }
    // Skip the word run.
    while let Some(&(ix, c)) = chars.peek() {
        if !is_word_char(c) {
            break;
        }
        pos = offset + ix + c.len_utf8();
        chars.next();
    }
    pos
}

/// Start of the current or previous word, mirroring `next_word_boundary`.
fn prev_word_boundary(text: &str, offset: usize) -> usize {
    let mut pos = offset;
    let before = |p: usize| text[..p].chars().next_back();
    // Skip the separator run.
    while let Some(c) = before(pos) {
        if is_word_char(c) {
            break;
        }
        pos -= c.len_utf8();
    }
    // Skip the word run.
    while let Some(c) = before(pos) {
        if !is_word_char(c) {
            break;
        }
        pos -= c.len_utf8();
    }
    pos
}

/// The word (or separator run) around `offset`, for double-click selection.
fn word_range_at(text: &str, offset: usize) -> std::ops::Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let offset = offset.min(text.len());
    let at = text[offset..]
        .chars()
        .next()
        .or_else(|| text[..offset].chars().next_back());
    let wordish = at.map(is_word_char).unwrap_or(false);

    let mut start = offset;
    while let Some(c) = text[..start].chars().next_back() {
        if is_word_char(c) != wordish {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = offset;
    for c in text[offset..].chars() {
        if is_word_char(c) != wordish {
            break;
        }
        end += c.len_utf8();
    }
    start..end
}

/// Emitted to the owning pane.
pub enum PathEditorEvent {
    /// Enter pressed; payload is the raw text. Validation happens in the pane.
    Committed(String),
    /// Escape, or focus left the editor.
    Cancelled,
    /// The text changed. The command palette re-matches on this; the address
    /// bar ignores it.
    Edited,
}

pub struct PathEditor {
    focus_handle: FocusHandle,
    content: SharedString,
    /// UTF-8 byte offsets, `start <= end` always; `selection_reversed` marks
    /// which end carries the cursor.
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// IME preedit span. Load-bearing on Wayland even without an IME.
    marked_range: Option<Range<usize>>,
    /// Written back by the element during paint; used for mouse hit tests and
    /// IME candidate placement.
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    is_selecting: bool,
    /// Set by the pane when a committed path wasn't a directory.
    pub error: bool,
    /// Drawn in place of the content while it is empty.
    placeholder: SharedString,
}

impl PathEditor {
    pub fn new(initial: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let len = initial.len();
        // Pre-select everything: the common case is replacing the text.
        Self::new_with_selection(initial, 0..len, window, cx)
    }

    /// `initial_selection` is a UTF-8 byte range. Rename uses this to select
    /// the file stem and leave the extension in place.
    pub fn new_with_selection(
        initial: String,
        initial_selection: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Focus loss cancels the edit; the pane restores its own focus.
        cx.on_blur(&focus_handle, window, |_, _, cx| {
            cx.emit(PathEditorEvent::Cancelled);
        })
        .detach();

        let len = initial.len();
        let selected_range = initial_selection.start.min(len)..initial_selection.end.min(len);
        Self {
            focus_handle,
            content: initial.into(),
            selected_range,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            is_selecting: false,
            error: false,
            placeholder: SharedString::default(),
        }
    }

    #[allow(dead_code)] // handy for tests/debugging of the widget
    pub fn text(&self) -> &str {
        &self.content
    }

    /// Grey prompt shown while the field is empty.
    pub fn with_placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    // ---- editing actions --------------------------------------------------

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.move_to(prev, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.move_to(next, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _window: &mut Window, cx: &mut Context<Self>) {
        let prev = self.previous_boundary(self.cursor_offset());
        self.select_to(prev, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _window: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_boundary(self.cursor_offset());
        self.select_to(next, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _window: &mut Window, cx: &mut Context<Self>) {
        let target = prev_word_boundary(&self.content, self.cursor_offset());
        self.move_to(target, cx);
    }

    fn word_right(&mut self, _: &WordRight, _window: &mut Window, cx: &mut Context<Self>) {
        let target = next_word_boundary(&self.content, self.cursor_offset());
        self.move_to(target, cx);
    }

    fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = prev_word_boundary(&self.content, self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = next_word_boundary(&self.content, self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_to_home(&mut self, _: &SelectToHome, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let target = prev_word_boundary(&self.content, self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let target = next_word_boundary(&self.content, self.cursor_offset());
            self.select_to(target, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // shape_line debug-asserts on newlines; a pasted path never needs them.
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PathEditorEvent::Committed(self.content.to_string()));
    }

    fn cancel(&mut self, _: &CancelEdit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PathEditorEvent::Cancelled);
    }

    // ---- selection plumbing ----------------------------------------------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.error = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .map(|(ix, _)| ix)
            .take_while(|&ix| ix < offset)
            .last()
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .map(|(ix, _)| ix)
            .find(|&ix| ix > offset)
            .unwrap_or(self.content.len())
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_x)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self.index_for_mouse_position(event.position);
        match event.click_count {
            2 => {
                let range = word_range_at(&self.content, index);
                self.selected_range = range;
                self.selection_reversed = false;
                cx.notify();
            }
            n if n >= 3 => {
                self.selected_range = 0..self.content.len();
                self.selection_reversed = false;
                cx.notify();
            }
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(index, cx);
                } else {
                    self.move_to(index, cx);
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let index = self.index_for_mouse_position(event.position);
            self.select_to(index, cx);
        }
    }

    // ---- UTF-16 offset conversion (the trait's contract) ------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for PathEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.take())
            .unwrap_or(self.selected_range.clone());

        self.content = format!(
            "{}{}{}",
            &self.content[0..range.start],
            new_text,
            &self.content[range.end..]
        )
        .into();
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.error = false;
        cx.emit(PathEditorEvent::Edited);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.take())
            .unwrap_or(self.selected_range.clone());

        self.content = format!(
            "{}{}{}",
            &self.content[0..range.start],
            new_text,
            &self.content[range.end..]
        )
        .into();
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.start)
            .unwrap_or_else(|| {
                let cursor = range.start + new_text.len();
                cursor..cursor
            });
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Places the IME candidate popup near the caret.
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start_x = line.x_for_index(range.start) - self.scroll_x;
        let end_x = line.x_for_index(range.end) - self.scroll_x;
        Some(Bounds::from_corners(
            point(element_bounds.left() + start_x, element_bounds.top()),
            point(element_bounds.left() + end_x, element_bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Never called by the Linux backends; the example's version is buggy.
        None
    }
}

impl Focusable for PathEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PathEditorEvent> for PathEditor {}
impl EventEmitter<DismissEvent> for PathEditor {}

impl Render for PathEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let border = if self.error {
            cx.theme().status().error
        } else {
            colors.border_selected
        };

        div()
            .key_context("AddressBar")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_to_home))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .cursor(gpui::CursorStyle::IBeam)
            .flex_1()
            .h(px(22.))
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(border)
            .bg(colors.editor_background)
            .overflow_hidden()
            .child(PathElement {
                editor: cx.entity(),
            })
    }
}

// ---------------------------------------------------------------------------
// The element: shapes the line, keeps the caret visible via scroll_x, paints
// selection/text/caret, and, load-bearing, re-registers the window input
// handler on every paint (registration lasts one frame and requires the focus
// handle to be *exactly* focused).
// ---------------------------------------------------------------------------

struct PathElement {
    editor: Entity<PathEditor>,
}

struct PathPrepaint {
    line: ShapedLine,
    scroll_x: Pixels,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for PathElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for PathElement {
    type RequestLayoutState = ();
    type PrepaintState = PathPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let editor = self.editor.read(cx);
        let showing_placeholder = editor.content.is_empty() && !editor.placeholder.is_empty();
        let content = if showing_placeholder {
            editor.placeholder.clone()
        } else {
            editor.content.clone()
        };
        let selected_range = editor.selected_range.clone();
        let cursor_offset = editor.cursor_offset();
        let mut scroll_x = editor.scroll_x;

        let style = window.text_style();
        let colors = cx.theme().colors();
        let text_color = if showing_placeholder {
            colors.text_muted
        } else {
            colors.text
        };
        let run = TextRun {
            len: content.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(content, font_size, &[run], None);

        // Keep the caret inside the field.
        let cursor_x = line.x_for_index(cursor_offset);
        let width = bounds.size.width;
        let caret_w = px(2.);
        if cursor_x - scroll_x < px(0.) {
            scroll_x = cursor_x;
        } else if cursor_x - scroll_x > width - caret_w {
            scroll_x = cursor_x - width + caret_w;
        }
        scroll_x = scroll_x
            .max(px(0.))
            .min((line.width - width + caret_w).max(px(0.)));

        let origin_x = bounds.left() - scroll_x;
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(origin_x + cursor_x, bounds.top()),
                        size(caret_w, bounds.size.height),
                    ),
                    text_color,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            origin_x + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            origin_x + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    cx.theme().colors().element_selection_background,
                )),
                None,
            )
        };

        PathPrepaint {
            line,
            scroll_x,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();

        // One-frame registration; must happen every paint, and only takes
        // effect while `focus_handle` is exactly focused.
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            let line = prepaint.line.clone();
            let origin = point(bounds.left() - prepaint.scroll_x, bounds.top());
            let _ = line.paint(
                origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });

        let line = prepaint.line.clone();
        let scroll_x = prepaint.scroll_x;
        self.editor.update(cx, |editor, _| {
            editor.last_layout = Some(line);
            editor.last_bounds = Some(bounds);
            editor.scroll_x = scroll_x;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_jumps_over_path_components() {
        let t = "/home/shaun/Projects";
        // From the start: land at the end of each component.
        assert_eq!(next_word_boundary(t, 0), 5); // "/home"
        assert_eq!(next_word_boundary(t, 5), 11); // "/shaun"
        assert_eq!(next_word_boundary(t, 11), 20); // "/Projects"
        assert_eq!(next_word_boundary(t, 20), 20); // at the end: stays
        // Backwards: land at the start of each component.
        assert_eq!(prev_word_boundary(t, 20), 12);
        assert_eq!(prev_word_boundary(t, 12), 6);
        assert_eq!(prev_word_boundary(t, 6), 1);
        assert_eq!(prev_word_boundary(t, 1), 0);
        assert_eq!(prev_word_boundary(t, 0), 0);
    }

    #[test]
    fn word_jumps_treat_dots_and_dashes_as_separators() {
        let t = "my-file.tar.gz";
        assert_eq!(next_word_boundary(t, 0), 2); // "my"
        assert_eq!(next_word_boundary(t, 2), 7); // "-file"
        assert_eq!(next_word_boundary(t, 7), 11); // ".tar"
        assert_eq!(next_word_boundary(t, 11), 14); // ".gz"
    }

    #[test]
    fn double_click_word_ranges() {
        let t = "/home/shaun/notes.txt";
        assert_eq!(word_range_at(t, 8), 6..11); // inside "shaun"
        assert_eq!(word_range_at(t, 0), 0..1); // on the leading slash
        assert_eq!(word_range_at(t, 21), 18..21); // at the end, in "txt"
    }

    #[test]
    fn word_boundaries_handle_unicode() {
        let t = "naïve-café";
        assert_eq!(next_word_boundary(t, 0), 6); // "naïve" (ï is 2 bytes)
        assert_eq!(prev_word_boundary(t, t.len()), 7); // start of "café"
    }
}
