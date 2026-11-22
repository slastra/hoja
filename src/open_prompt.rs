//! A confirmation before opening a file that lives inside an archive.
//!
//! Opening one means extracting a temporary copy: nothing written back to it
//! reaches the archive, which is the trap every archive tool has shipped and
//! regretted (see `dir_pane::activate`). This asks once, rather than doing
//! that silently.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window,
    prelude::*,
};

use crate::ui::{self, Cancel, Confirm, SelectNext, SelectPrevious};

/// The answers, lit one first.
const ANSWERS: [(&str, &str, OpenPromptEvent); 2] = [
    ("confirm", "Extract and Open", OpenPromptEvent::Confirmed),
    ("cancel", "Cancel", OpenPromptEvent::Cancelled),
];

pub struct OpenPrompt {
    file_name: String,
    /// The answer Enter takes. An index into `ANSWERS`.
    active: usize,
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
            active: 0,
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

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(ANSWERS[self.active].2, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = ui::step_answer(self.active, ANSWERS.len(), true);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active = ui::step_answer(self.active, ANSWERS.len(), false);
        cx.notify();
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
        let answers = ANSWERS
            .iter()
            .enumerate()
            .map(|(at, (id, label, event))| {
                let event = *event;
                ui::prompt_answer(*id, *label, at == self.active, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.decide(event, cx)))
                    .into_any_element()
            })
            .collect();
        ui::prompt(
            format!("Open \u{201c}{}\u{201d}?", self.file_name),
            "This extracts a temporary copy to open it. Changes you make there will not be \
             saved back into the archive.",
            None,
            answers,
            cx,
        )
        .track_focus(&self.focus_handle)
        .key_context(ui::PROMPT_CONTEXT)
        .on_action(cx.listener(Self::cancel))
        .on_action(cx.listener(Self::confirm))
        .on_action(cx.listener(Self::select_next))
        .on_action(cx.listener(Self::select_previous))
    }
}
