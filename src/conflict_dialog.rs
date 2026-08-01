//! Themed replacement for the fallback prompt on file conflicts.
//!
//! One dialog per conflict. "Apply to all" is a checkbox rather than doubled
//! buttons, so the choice row stays short: Cancel / Skip / Keep Both / Replace.
//! The decision goes straight into the engine's reply channel; the workspace
//! only hears `DismissEvent` and shows the next queued conflict, if any.

use std::path::Path;
use std::sync::mpsc;

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Subscription, Window, div,
    prelude::*, px,
};
use pane_transfer::{ConflictChoice, ConflictDecision};
use theme::ActiveTheme;

use crate::file_menu::{Cancel, Confirm};
use crate::icon::Icon;

pub struct ConflictDialog {
    file_name: String,
    dest_folder: String,
    reply: mpsc::Sender<ConflictDecision>,
    apply_to_all: bool,
    focus_handle: FocusHandle,
    _blur_subscription: Subscription,
}

impl ConflictDialog {
    pub fn new(
        src: &Path,
        dest: &Path,
        reply: mpsc::Sender<ConflictDecision>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _ = src;
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
            focus_handle,
            _blur_subscription,
        }
    }

    fn send(&mut self, decision: ConflictDecision, cx: &mut Context<Self>) {
        let _ = self.reply.send(decision);
        cx.emit(DismissEvent);
    }

    fn decide(&mut self, choice: ConflictChoice, cx: &mut Context<Self>) {
        let decision = ConflictDecision::Apply {
            choice,
            apply_to_all: self.apply_to_all,
        };
        self.send(decision, cx);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.send(ConflictDecision::CancelJob, cx);
    }

    /// Enter = Replace, the convention for the highlighted default action.
    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.decide(ConflictChoice::Overwrite, cx);
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
        let colors = cx.theme().colors();
        let text = colors.text;
        let muted = colors.text_muted;
        let hover_bg = colors.element_hover;
        let apply_to_all = self.apply_to_all;

        let button = |id: &'static str,
                      label: &'static str,
                      primary: bool,
                      choice: ConflictChoice,
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
                .on_click(cx.listener(move |this, _, _, cx| this.decide(choice, cx)))
        };

        div()
            .occlude()
            .track_focus(&self.focus_handle)
            .key_context("menu")
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
                    .child(format!("Replace \u{201c}{}\u{201d}?", self.file_name)),
            )
            .child(div().text_color(muted).child(format!(
                "An item with this name already exists in \u{201c}{}\u{201d}.",
                self.dest_folder
            )))
            .child(
                div()
                    .id("apply-to-all")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .text_color(muted)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.apply_to_all = !this.apply_to_all;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(14.))
                            .flex_none()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(apply_to_all, |el| {
                                el.child(Icon::from_path("icons/file_icons/check.svg", text))
                            }),
                    )
                    .child("Apply to all remaining conflicts"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .id("cancel")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(move |s| s.bg(hover_bg))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send(ConflictDecision::CancelJob, cx)
                            })),
                    )
                    .child(button("skip", "Skip", false, ConflictChoice::Skip, cx))
                    .child(button(
                        "keep-both",
                        "Keep Both",
                        false,
                        ConflictChoice::KeepBoth,
                        cx,
                    ))
                    .child(button(
                        "replace",
                        "Replace",
                        true,
                        ConflictChoice::Overwrite,
                        cx,
                    )),
            )
    }
}
