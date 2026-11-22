//! The question a transfer asks when a name is taken.
//!
//! Drawn as Zed draws a prompt (see `ui::prompt`): the question, the answers
//! stacked under it, the arrows and Tab moving between them and Enter taking
//! the lit one. "Apply to all" is a checkbox rather than doubled answers. The
//! decision goes straight into the engine's reply channel; the workspace only
//! hears `DismissEvent` and shows the next queued conflict, if any.

use std::path::Path;
use std::sync::mpsc;

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window,
    prelude::*,
};
use hoja_transfer::{ConflictChoice, ConflictDecision};

use crate::ui::{self, Cancel, Confirm, SelectNext, SelectPrevious};

/// The answers, in the order they are listed. Replace is first, so it is lit
/// when the prompt opens and Enter takes it, as it always has.
const ANSWERS: [(&str, &str, Option<ConflictChoice>); 4] = [
    ("replace", "Replace", Some(ConflictChoice::Overwrite)),
    ("keep-both", "Keep Both", Some(ConflictChoice::KeepBoth)),
    ("skip", "Skip", Some(ConflictChoice::Skip)),
    // Not a choice about this file: it stops the whole transfer.
    ("cancel", "Cancel", None),
];

pub struct ConflictDialog {
    file_name: String,
    dest_folder: String,
    reply: mpsc::Sender<ConflictDecision>,
    apply_to_all: bool,
    /// The answer Enter takes. An index into `ANSWERS`.
    active: usize,
    focus_handle: FocusHandle,
    _blur_subscription: Subscription,
}

impl ConflictDialog {
    pub fn new(
        dest: &Path,
        reply: mpsc::Sender<ConflictDecision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Focus loss must not leave the worker hanging: treat it as Skip for
        // this one file, keeping both the job and the data intact.
        let _blur_subscription = cx.on_blur(&focus_handle, window, |this: &mut Self, _, cx| {
            this.send(
                ConflictDecision::Apply {
                    choice: ConflictChoice::Skip,
                    apply_to_all: false,
                },
                cx,
            );
        });

        Self {
            file_name: dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dest_folder: dest
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "this folder".to_string()),
            reply,
            apply_to_all: false,
            active: 0,
            focus_handle,
            _blur_subscription,
        }
    }

    fn send(&mut self, decision: ConflictDecision, cx: &mut Context<Self>) {
        let _ = self.reply.send(decision);
        cx.emit(DismissEvent);
    }

    fn apply(&self, choice: ConflictChoice) -> ConflictDecision {
        ConflictDecision::Apply {
            choice,
            apply_to_all: self.apply_to_all,
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.send(ConflictDecision::CancelJob, cx);
    }

    fn answer(&mut self, at: usize, cx: &mut Context<Self>) {
        let decision = match ANSWERS[at].2 {
            Some(choice) => self.apply(choice),
            None => ConflictDecision::CancelJob,
        };
        self.send(decision, cx);
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.answer(self.active, cx);
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

impl Focusable for ConflictDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ConflictDialog {}

impl Render for ConflictDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let answers = ANSWERS
            .iter()
            .enumerate()
            .map(|(at, (id, label, _))| {
                ui::prompt_answer(*id, *label, at == self.active, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.answer(at, cx)))
                    .into_any_element()
            })
            .collect();
        let apply_to_all = ui::checkbox(
            "apply-to-all",
            self.apply_to_all,
            "Apply to all remaining conflicts",
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.apply_to_all = !this.apply_to_all;
            cx.notify();
        }))
        .into_any_element();

        ui::prompt(
            format!("Replace \u{201c}{}\u{201d}?", self.file_name),
            format!(
                "An item with this name already exists in \u{201c}{}\u{201d}.",
                self.dest_folder
            ),
            Some(apply_to_all),
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
