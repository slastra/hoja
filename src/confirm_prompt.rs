//! A question with one answer to go on and a Cancel, asked before something
//! the user may not expect.
//!
//! - Before opening a file that lives inside an archive. Opening one means
//!   extracting a temporary copy: nothing written back to it reaches the
//!   archive, which is the trap every archive tool has shipped and regretted
//!   (see `dir_pane::activate`). This asks once, rather than doing that
//!   silently.
//! - Before a picker saves over a file that is there already, as the GTK
//!   dialog asks.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window,
    prelude::*,
};

use crate::ui::{self, Cancel, Confirm, SelectNext, SelectPrevious};

/// The answers, lit one first. The first one's label is the prompt's own.
const ANSWERS: [(&str, ConfirmEvent); 2] = [
    ("confirm", ConfirmEvent::Confirmed),
    ("cancel", ConfirmEvent::Cancelled),
];

pub struct ConfirmPrompt {
    /// What the probe calls it, so a test can tell one question from another.
    pub kind: &'static str,
    message: String,
    detail: String,
    /// The label of the answer that goes on.
    confirm: &'static str,
    /// The answer Enter takes. An index into `ANSWERS`.
    active: usize,
    focus_handle: FocusHandle,
    _blur_subscription: Subscription,
}

/// What the user decided. Losing focus counts as `Cancelled`: leaving a
/// question on screen and clicking elsewhere is a clearer "never mind" than
/// extracting a copy nobody asked for.
#[derive(Clone, Copy)]
pub enum ConfirmEvent {
    Confirmed,
    Cancelled,
}

impl ConfirmPrompt {
    /// Before opening `file_name`, a member of an archive.
    pub fn extract(file_name: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(
            "open-prompt",
            format!("Open \u{201c}{file_name}\u{201d}?"),
            "This extracts a temporary copy to open it. Changes you make there will not be \
             saved back into the archive."
                .to_string(),
            "Extract and Open",
            window,
            cx,
        )
    }

    /// Before a save writes over `names`, which are in `folder` already.
    pub fn replace(
        names: &[String],
        folder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (message, detail) = match names {
            [one] => (
                format!("Replace \u{201c}{one}\u{201d}?"),
                format!(
                    "A file with this name is in \u{201c}{folder}\u{201d} already. \
                     Saving replaces its contents."
                ),
            ),
            several => (
                format!("Replace {} files?", several.len()),
                format!(
                    "Files with these names are in \u{201c}{folder}\u{201d} already: {}. \
                     Saving replaces their contents.",
                    several.join(", ")
                ),
            ),
        };
        Self::new("replace-prompt", message, detail, "Replace", window, cx)
    }

    fn new(
        kind: &'static str,
        message: String,
        detail: String,
        confirm: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let _blur_subscription = cx.on_blur(&focus_handle, window, |this: &mut Self, _, cx| {
            this.decide(ConfirmEvent::Cancelled, cx);
        });

        Self {
            kind,
            message,
            detail,
            confirm,
            active: 0,
            focus_handle,
            _blur_subscription,
        }
    }

    fn decide(&mut self, event: ConfirmEvent, cx: &mut Context<Self>) {
        cx.emit(event);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(ConfirmEvent::Cancelled, cx);
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(ANSWERS[self.active].1, cx);
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

impl Focusable for ConfirmPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ConfirmEvent> for ConfirmPrompt {}
impl EventEmitter<DismissEvent> for ConfirmPrompt {}

impl Render for ConfirmPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let answers = ANSWERS
            .iter()
            .enumerate()
            .map(|(at, (id, event))| {
                let event = *event;
                let label = match event {
                    ConfirmEvent::Confirmed => self.confirm,
                    ConfirmEvent::Cancelled => "Cancel",
                };
                ui::prompt_answer(*id, label, at == self.active, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.decide(event, cx)))
                    .into_any_element()
            })
            .collect();
        ui::prompt(self.message.clone(), self.detail.clone(), None, answers, cx)
            .track_focus(&self.focus_handle)
            .key_context(ui::PROMPT_CONTEXT)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
    }
}
