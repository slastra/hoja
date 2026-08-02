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
use hoja_transfer::{ConflictChoice, ConflictDecision};
use theme::ActiveTheme;

use crate::icon::Icon;

// The dialog has its own context so menu-navigation bindings cannot leak into
// it, and so changing menu keys cannot silently change dialog behavior.
gpui::actions!(dialog, [Cancel, Confirm]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonStyle {
    Primary,
    Normal,
    Ghost,
}

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

    /// Enter = Replace, the convention for the highlighted default action.
    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let decision = self.apply(ConflictChoice::Overwrite);
        self.send(decision, cx);
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

        // `ghost` is the borderless Cancel; `primary` is the default action.
        let button = |id: &'static str,
                      label: &'static str,
                      style: ButtonStyle,
                      decision: ConflictDecision,
                      cx: &Context<Self>| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .when(style != ButtonStyle::Ghost, |el| {
                    el.border_1().border_color(if style == ButtonStyle::Primary {
                        colors.border_selected
                    } else {
                        colors.border
                    })
                })
                .when(style == ButtonStyle::Primary, |el| {
                    el.bg(colors.element_selected)
                })
                .when(style == ButtonStyle::Ghost, |el| el.text_color(muted))
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.send(decision, cx)))
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
                            .size(px(16.))
                            .flex_none()
                            .rounded_sm()
                            // The box must read as a control in both states: an
                            // inset fill plus a border that brightens with use.
                            .bg(colors.editor_background)
                            .border_1()
                            .border_color(if apply_to_all {
                                colors.border_selected
                            } else {
                                muted
                            })
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
                    // Bottom action rows group at the right, Cancel leftmost
                    // within the group.
                    .child(button(
                        "cancel",
                        "Cancel",
                        ButtonStyle::Ghost,
                        ConflictDecision::CancelJob,
                        cx,
                    ))
                    .child(button(
                        "skip",
                        "Skip",
                        ButtonStyle::Normal,
                        self.apply(ConflictChoice::Skip),
                        cx,
                    ))
                    .child(button(
                        "keep-both",
                        "Keep Both",
                        ButtonStyle::Normal,
                        self.apply(ConflictChoice::KeepBoth),
                        cx,
                    ))
                    .child(button(
                        "replace",
                        "Replace",
                        ButtonStyle::Primary,
                        self.apply(ConflictChoice::Overwrite),
                        cx,
                    )),
            )
    }
}
