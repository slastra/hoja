//! A confirmation before opening a file that lives inside an archive.
//!
//! Opening one means extracting a temporary copy: nothing written back to it
//! reaches the archive, which is the trap every archive tool has shipped and
//! regretted (see `dir_pane::activate`). This asks once, rather than doing
//! that silently.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window, div,
    prelude::*, px,
};
use theme::ActiveTheme;

// Shared with the conflict dialog rather than declared again: only one of
// these overlays is ever up at a time, and a second Cancel/Confirm pair would
// be a second thing to bind at the same "dialog" context for no reason.
use crate::conflict_dialog::{Cancel, Confirm};

pub struct OpenPrompt {
    file_name: String,
    focus_handle: FocusHandle,
    _blur_subscription: Subscription,
}

/// What the user decided. Losing focus counts as `Cancelled`: leaving a
/// question on screen and clicking elsewhere is a clearer "never mind" than
/// extracting a copy nobody asked for.
#[derive(Clone, Copy)]
pub enum OpenPromptEvent {
    Confirmed,
    Cancelled,
}

impl OpenPrompt {
    pub fn new(file_name: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let _blur_subscription = cx.on_blur(&focus_handle, window, |this: &mut Self, _, cx| {
            this.decide(OpenPromptEvent::Cancelled, cx);
        });

        Self {
            file_name,
            focus_handle,
            _blur_subscription,
        }
    }

    fn decide(&mut self, event: OpenPromptEvent, cx: &mut Context<Self>) {
        cx.emit(event);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(OpenPromptEvent::Cancelled, cx);
    }

    /// Enter = extract and open, the convention for the highlighted default.
    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(OpenPromptEvent::Confirmed, cx);
    }
}

impl Focusable for OpenPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<OpenPromptEvent> for OpenPrompt {}
impl EventEmitter<DismissEvent> for OpenPrompt {}

impl Render for OpenPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let text = colors.text;
        let muted = colors.text_muted;
        let hover_bg = colors.element_hover;

        let button = |id: &'static str,
                      label: &'static str,
                      primary: bool,
                      event: OpenPromptEvent,
                      cx: &Context<Self>| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if primary {
                    colors.border_selected
                } else {
                    colors.border
                })
                .when(primary, |el| el.bg(colors.element_selected))
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.decide(event, cx)))
        };

        div()
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("dialog")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .flex()
            .flex_col()
            .gap_3()
            .w(px(440.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .text_sm()
            .text_color(text)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .truncate()
                    .child(format!("Open \u{201c}{}\u{201d}?", self.file_name)),
            )
            .child(
                div().text_color(muted).child(
                    "This extracts a temporary copy to open it. Changes you make there will \
                     not be saved back into the archive."
                        .to_string(),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(button(
                        "cancel",
                        "Cancel",
                        false,
                        OpenPromptEvent::Cancelled,
                        cx,
                    ))
                    .child(button(
                        "confirm",
                        "Extract & Open",
                        true,
                        OpenPromptEvent::Confirmed,
                        cx,
                    )),
            )
    }
}
