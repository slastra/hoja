//! What a transfer could not do, listed in full.
//!
//! The job strip has one line, so a failed transfer could only ever say how
//! many files failed and name the first. That is the wrong one to name: the
//! first error is rarely the interesting one, and a job can fail in several
//! ways at once — a directory that is unreadable, a filesystem that refuses
//! symlinks, a disk that filled up halfway. This is the whole list.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Window, actions, div,
    prelude::*, px, uniform_list,
};
use std::path::PathBuf;
use theme::ActiveTheme;

use crate::icon::Icon;

actions!(failures, [Dismiss]);

/// The key context the modal declares, so escape reaches it.
pub const KEY_CONTEXT: &str = "failures";

const ROW_HEIGHT: f32 = 40.;
const MAX_VISIBLE_ROWS: f32 = 12.;

/// One thing that went wrong, as the strip collected it.
#[derive(Clone, Debug)]
pub struct Failure {
    pub path: PathBuf,
    /// Already rendered by the engine's `Display`, which reads
    /// "Symlink: Operation not permitted".
    pub reason: String,
}

/// The deepest directory every failure sits under, so the rows can drop it.
///
/// A transfer that fails does so on a run of files in the same few directories,
/// and their full paths are near-identical for the first hundred characters —
/// which is exactly the part a row has room for. Printed whole, seven rows read
/// as seven copies of the same truncated prefix and no filenames at all. Lifted
/// out, the rows carry only what distinguishes them.
///
/// `None` when they share nothing worth removing, which leaves the paths whole.
fn common_root(failures: &[Failure]) -> Option<PathBuf> {
    let mut dirs = failures.iter().filter_map(|f| f.path.parent());
    let mut root = dirs.next()?.to_path_buf();
    for dir in dirs {
        // Walk up until the candidate covers this one too.
        while !dir.starts_with(&root) {
            // False at the filesystem root, where there is nothing shared.
            if !root.pop() {
                return None;
            }
        }
    }
    // "/" and "" are not worth a header line of their own.
    root.parent().is_some().then_some(root)
}

pub struct FailureReport {
    /// The job's own label, so the modal says which transfer this was.
    label: String,
    failures: Vec<Failure>,
    /// How many there were in total, which is more than `failures` holds when a
    /// job failed thousands of times — see `MAX_RETAINED_FAILURES`.
    total: usize,
    /// Trimmed off the front of every row, and shown once instead.
    root: Option<PathBuf>,
    scroll: gpui::UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl FailureReport {
    pub fn new(label: String, failures: Vec<Failure>, total: usize, cx: &mut Context<Self>) -> Self {
        Self {
            label,
            root: common_root(&failures),
            failures,
            total,
            scroll: gpui::UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_row(&self, ix: usize, cx: &Context<Self>) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let Some(failure) = self.failures.get(ix) else {
            return div().into_any_element();
        };
        let path = match &self.root {
            Some(root) => failure.path.strip_prefix(root).unwrap_or(&failure.path),
            None => &failure.path,
        };
        div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .flex_col()
            .justify_center()
            .gap_0p5()
            .child(
                div()
                    .truncate()
                    .text_sm()
                    .text_color(colors.text)
                    // The path is what identifies the file; it goes first and
                    // gets the readable colour.
                    .child(path.display().to_string()),
            )
            .child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(failure.reason.clone()),
            )
            .into_any_element()
    }
}

impl Focusable for FailureReport {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FailureReport {}

impl Render for FailureReport {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let shown = self.failures.len();
        let heading = match self.total {
            1 => "1 file could not be transferred".to_string(),
            n => format!(
                "{} files could not be transferred",
                crate::notifications::count(n as u64)
            ),
        };

        let list = uniform_list(
            "failures",
            shown,
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        // On the list itself, never a parent: it virtualizes, so it builds
        // however many rows fit *its own* height and a height on an ancestor
        // tells it nothing.
        .h(px(ROW_HEIGHT * (shown as f32).min(MAX_VISIBLE_ROWS)));

        div()
            .occlude()
            .flex()
            .flex_col()
            .w(px(720.))
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::dismiss))
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Icon::from_path(
                        "icons/file_icons/warning.svg",
                        cx.theme().status().error,
                    ))
                    .child(div().flex_1().text_sm().text_color(colors.text).child(heading))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .child(self.label.clone()),
                    ),
            )
            .when_some(self.root.clone(), |el, root| {
                // Said once, since every row below had it removed.
                el.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .truncate()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .border_b_1()
                        .border_color(colors.border)
                        .child(format!("in {}", root.display())),
                )
            })
            .child(list)
            .when(shown < self.total, |el| {
                // The strip stops collecting past a cap, so say so rather than
                // letting the list imply it is the whole story.
                el.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(colors.border)
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(format!(
                            "and {} more",
                            crate::notifications::count((self.total - shown) as u64)
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn failed(paths: &[&str]) -> Vec<Failure> {
        paths
            .iter()
            .map(|p| Failure {
                path: PathBuf::from(p),
                reason: "Open: Permission denied".into(),
            })
            .collect()
    }

    #[test]
    fn one_directory_lifts_out_entirely() {
        let root = common_root(&failed(&["/src/tree/a.bin", "/src/tree/b.bin"]));
        assert_eq!(root, Some(PathBuf::from("/src/tree")));
    }

    #[test]
    fn subdirectories_keep_the_part_that_differs() {
        let failures = failed(&["/src/tree/one/a.bin", "/src/tree/two/b.bin"]);
        let root = common_root(&failures).unwrap();
        assert_eq!(root, PathBuf::from("/src/tree"));
        assert_eq!(
            failures[0].path.strip_prefix(&root).unwrap(),
            Path::new("one/a.bin")
        );
    }

    #[test]
    fn paths_sharing_only_the_filesystem_root_keep_their_length() {
        // Nothing to lift out, and "in /" would be a line saying nothing.
        assert_eq!(common_root(&failed(&["/one/a.bin", "/two/b.bin"])), None);
    }

    #[test]
    fn a_lone_failure_still_names_its_directory() {
        assert_eq!(
            common_root(&failed(&["/src/tree/a.bin"])),
            Some(PathBuf::from("/src/tree"))
        );
    }

    #[test]
    fn no_failures_means_no_root() {
        assert_eq!(common_root(&[]), None);
    }

    #[test]
    fn a_prefix_that_is_not_a_path_component_does_not_count() {
        // "/src/tree" is a textual prefix of "/src/treehouse" but not a parent
        // of it; strip_prefix would fail on the second and print it whole.
        let root = common_root(&failed(&["/src/tree/a.bin", "/src/treehouse/b.bin"]));
        assert_eq!(root, Some(PathBuf::from("/src")));
    }
}
