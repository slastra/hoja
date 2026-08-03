//! What a transfer could not do, grouped by why.
//!
//! The job strip has one line, so a failed transfer could only ever say how
//! many files failed and name the first. That is the wrong one to name: the
//! first error is rarely the interesting one, and a job can fail in several
//! ways at once, a directory that is unreadable, a filesystem that refuses
//! symlinks, a disk that filled up halfway.
//!
//! Listing each failure with its own reason attached reads badly at the scale
//! failures actually arrive: copying a source tree onto exFAT produced 2,619 of
//! them, every one saying "Operation not permitted", and the answer the list was
//! being asked for, *what went wrong*, was buried under 2,619 repetitions of
//! itself. So the reason is the heading and the files sit under it.

use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Window, actions, div,
    prelude::*, px, uniform_list,
};
use std::collections::HashMap;
use std::path::PathBuf;
use theme::ActiveTheme;

use crate::icon::Icon;

actions!(failures, [Dismiss]);

/// The key context the modal declares, so escape reaches it.
pub const KEY_CONTEXT: &str = "failures";

const LINE_HEIGHT: f32 = 22.;
const MAX_VISIBLE_LINES: f32 = 18.;
const MODAL_W: f32 = 780.;

/// Files listed under one reason before the rest become a count.
///
/// Without a per-group cap the largest group buries every other one: a job that
/// fails 2,000 times on symlinks and once on a full disk would need two thousand
/// lines of scrolling before the disk was mentioned, and the disk is the one
/// worth knowing about.
///
/// Small, because this is a diagnosis and not an inventory. Eight names are
/// enough to recognise which files a reason is catching; the heading's count
/// carries the rest, and every other reason stays on the same screen.
const MAX_FILES_PER_GROUP: usize = 8;

/// One thing that went wrong, as the strip collected it.
#[derive(Clone, Debug)]
pub struct Failure {
    pub path: PathBuf,
    /// Already rendered by the engine's `Display`, which reads
    /// "Symlink: Operation not permitted".
    pub reason: String,
}

/// A reason with the files it accounts for, ready to lay out as flat lines.
///
/// Flat because `uniform_list` virtualizes on a single row height, and a job
/// with thousands of failures is exactly the one that needs virtualizing.
enum Line {
    Reason { text: String, count: usize },
    File(String),
    /// The tail of a group that ran past `MAX_FILES_PER_GROUP`.
    More(usize),
}

/// Drop the errno that `io::Error` prints after its own message.
///
/// "File name too long (os error 36)" is the same fact twice, and the half in
/// parentheses is the half nobody reads. It also keeps the parenthetical out of
/// the grouping key, where it would have been a silent way to split one reason
/// into two.
pub fn tidy(reason: &str) -> &str {
    match reason.rfind(" (os error ") {
        Some(ix) if reason.ends_with(')') => &reason[..ix],
        _ => reason,
    }
}

/// The deepest directory every failure sits under, so the rows can drop it.
///
/// A transfer that fails does so on a run of files in the same few directories,
/// and their full paths are near-identical for the first hundred characters,
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
    /// The whole report, pre-laid-out: reasons with their files beneath them.
    lines: Vec<Line>,
    /// How many failed in total, which exceeds what `lines` accounts for only
    /// when a job blew past `MAX_RETAINED_FAILURES`.
    total: usize,
    /// Held back from every path below, and shown once instead.
    root: Option<PathBuf>,
    scroll: gpui::UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl FailureReport {
    pub fn new(
        label: String,
        failures: Vec<Failure>,
        counts: HashMap<String, usize>,
        total: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = common_root(&failures);
        Self {
            label,
            lines: group(&counts, &failures, root.as_deref()),
            total,
            root,
            scroll: gpui::UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_line(&self, ix: usize, cx: &Context<Self>) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let line = div().h(px(LINE_HEIGHT)).w_full().flex().items_center();
        match self.lines.get(ix) {
            // Flush left and at full strength against indented, muted files:
            // in a grouped report the reason is the finding and the names under
            // it are the evidence. A background band said this more quietly,
            // and in themes where the two surface colours are close, not at all.
            Some(Line::Reason { text, count }) => line
                .px_3()
                .gap_2()
                .text_sm()
                .text_color(colors.text)
                // Every heading but the first, which already has the header's.
                .when(ix > 0, |el| el.border_t_1().border_color(colors.border))
                .child(div().flex_1().truncate().child(text.clone()))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(match count {
                            1 => "1 file".to_string(),
                            n => format!("{} files", crate::notifications::count(*n as u64)),
                        }),
                )
                .into_any_element(),
            Some(Line::File(path)) => line
                .pl_6()
                .pr_3()
                .text_sm()
                .text_color(colors.text_muted)
                .child(div().truncate().child(path.clone()))
                .into_any_element(),
            Some(Line::More(n)) => line
                .pl_6()
                .pr_3()
                .text_xs()
                .text_color(colors.text_muted)
                .child(format!(
                    "and {} more",
                    crate::notifications::count(*n as u64)
                ))
                .into_any_element(),
            None => line.into_any_element(),
        }
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
        let heading = match self.total {
            1 => "1 file could not be transferred".to_string(),
            n => format!(
                "{} files could not be transferred",
                crate::notifications::count(n as u64)
            ),
        };
        let list = uniform_list(
            "failures",
            self.lines.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                range.map(|ix| this.render_line(ix, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        // On the list itself, never a parent: it virtualizes, so it builds
        // however many rows fit *its own* height and a height on an ancestor
        // tells it nothing.
        .h(px(LINE_HEIGHT * (self.lines.len() as f32).min(MAX_VISIBLE_LINES)));

        div()
            .occlude()
            .flex()
            .flex_col()
            .w(px(MODAL_W))
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
                    .items_start()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(div().flex_none().pt_0p5().child(Icon::from_path(
                        "icons/file_icons/warning.svg",
                        cx.theme().status().error,
                    )))
                    .child(
                        // Stacked, not side by side: a transfer to a freshly
                        // mounted volume is labelled with its UUID, and across
                        // one line that squeezed the heading to nothing.
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().text_color(colors.text).child(heading))
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .child(self.label.clone()),
                            ),
                    ),
            )
            .child(list)
            .when_some(self.root.clone(), |el, root| {
                el.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .border_t_1()
                        .border_color(colors.border)
                        .text_xs()
                        .text_color(colors.text_muted)
                        // Said once here, since every path above had it cut.
                        // Nothing about a remainder: each heading carries its
                        // own true count, and says under itself how many of its
                        // files went unnamed.
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .child(format!("in {}", root.display())),
                        ),
                )
            })
    }
}

/// Lay the failures out as reason headings with their files beneath.
///
/// Biggest group first, because the reason that accounts for most of a failed
/// transfer is the one that explains it. Ties break on the reason text so the
/// order does not shuffle between two renders of the same report.
///
/// `counts` is every reason and its true count; `examples` are the paths the
/// strip still holds, which is a prefix of the failures in arrival order. The
/// two are separate because they have to be: a job that fails forty thousand
/// times on symlinks and then fills the disk keeps no path from the second
/// reason at all, and grouping the paths alone would have dropped the heading
/// with them. The disk filling up is the one this report exists to surface.
fn group(
    counts: &HashMap<String, usize>,
    examples: &[Failure],
    root: Option<&std::path::Path>,
) -> Vec<Line> {
    let mut by_reason: HashMap<&str, Vec<String>> = HashMap::new();
    for failure in examples {
        let paths = by_reason.entry(tidy(&failure.reason)).or_default();
        if paths.len() == MAX_FILES_PER_GROUP {
            continue;
        }
        let path = match root {
            Some(root) => failure.path.strip_prefix(root).unwrap_or(&failure.path),
            None => &failure.path,
        };
        paths.push(path.display().to_string());
    }

    let mut order: Vec<&str> = counts.keys().map(String::as_str).collect();
    order.sort_by(|a, b| counts[*b].cmp(&counts[*a]).then_with(|| a.cmp(b)));

    let mut lines = Vec::new();
    for reason in order {
        let count = counts[reason];
        let files = by_reason.get(reason).map(Vec::as_slice).unwrap_or(&[]);
        lines.push(Line::Reason {
            text: reason.to_string(),
            count,
        });
        lines.extend(files.iter().cloned().map(Line::File));
        if count > files.len() {
            lines.push(Line::More(count - files.len()));
        }
    }
    lines
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

#[cfg(test)]
mod grouping_tests {
    use super::*;
    use std::path::Path;

    /// Counts derived from the examples, which is the un-capped case.
    fn group_all(failures: &[Failure], root: Option<&Path>) -> Vec<Line> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for failure in failures {
            *counts.entry(tidy(&failure.reason).to_string()).or_default() += 1;
        }
        group(&counts, failures, root)
    }

    fn failed(pairs: &[(&str, &str)]) -> Vec<Failure> {
        pairs
            .iter()
            .map(|(path, reason)| Failure {
                path: PathBuf::from(path),
                reason: (*reason).to_string(),
            })
            .collect()
    }

    fn counts_of(pairs: &[(&str, usize)]) -> HashMap<String, usize> {
        pairs
            .iter()
            .map(|(reason, n)| ((*reason).to_string(), *n))
            .collect()
    }

    fn reasons(lines: &[Line]) -> Vec<(String, usize)> {
        lines
            .iter()
            .filter_map(|line| match line {
                Line::Reason { text, count } => Some((text.clone(), *count)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_errno_in_parentheses_is_dropped() {
        assert_eq!(tidy("Open: File name too long (os error 36)"), "Open: File name too long");
    }

    #[test]
    fn a_reason_that_never_had_an_errno_is_untouched() {
        assert_eq!(tidy("Open: special files are not copied"), "Open: special files are not copied");
    }

    #[test]
    fn the_same_reason_with_different_errnos_still_groups_as_one() {
        // Only reachable if two errno values ever print the same message, but
        // the point is that the grouping key is the message and not the number.
        let lines = group_all(
            &failed(&[
                ("/a", "Open: Permission denied (os error 13)"),
                ("/b", "Open: Permission denied (os error 1)"),
            ]),
            None,
        );
        assert_eq!(reasons(&lines), vec![("Open: Permission denied".into(), 2)]);
    }

    #[test]
    fn the_reason_accounting_for_most_failures_comes_first() {
        // The disk filling up is one line and the symlinks are three; the
        // heading order is what tells you which one explains the transfer.
        let lines = group_all(
            &failed(&[
                ("/t/a", "Symlink: Operation not permitted"),
                ("/t/full", "Write: No space left on device"),
                ("/t/b", "Symlink: Operation not permitted"),
                ("/t/c", "Symlink: Operation not permitted"),
            ]),
            None,
        );
        assert_eq!(
            reasons(&lines),
            vec![
                ("Symlink: Operation not permitted".into(), 3),
                ("Write: No space left on device".into(), 1),
            ]
        );
    }

    #[test]
    fn a_group_past_the_cap_ends_in_a_count_of_the_rest() {
        let many: Vec<(String, &str)> = (0..MAX_FILES_PER_GROUP + 5)
            .map(|i| (format!("/t/f{i}"), "Open: Permission denied"))
            .collect();
        let lines = group_all(
            &failed(
                &many
                    .iter()
                    .map(|(p, r)| (p.as_str(), *r))
                    .collect::<Vec<_>>(),
            ),
            None,
        );
        let files = lines.iter().filter(|l| matches!(l, Line::File(_))).count();
        assert_eq!(files, MAX_FILES_PER_GROUP, "files shown");
        assert!(
            matches!(lines.last(), Some(Line::More(5))),
            "the rest are counted, not dropped silently"
        );
        // The heading still reports every one of them.
        assert_eq!(reasons(&lines)[0].1, MAX_FILES_PER_GROUP + 5);
    }

    #[test]
    fn a_reason_with_no_surviving_paths_still_gets_a_heading() {
        // The case the split exists for: the strip stops keeping paths after
        // MAX_RETAINED_FAILURES, so a job that fails forty thousand times on
        // symlinks and then fills the disk holds no path for the disk at all.
        // Grouping the paths alone lost the heading with them, and the disk
        // filling up is what the report exists to surface.
        let counts = counts_of(&[
            ("Symlink: Operation not permitted", 40_000),
            ("Write: No space left on device", 5_000),
        ]);
        let examples = failed(&[("/t/a", "Symlink: Operation not permitted")]);
        let lines = group(&counts, &examples, None);

        assert_eq!(
            reasons(&lines),
            vec![
                ("Symlink: Operation not permitted".into(), 40_000),
                ("Write: No space left on device".into(), 5_000),
            ]
        );
        // And the heading that kept no paths says so rather than sitting empty.
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, Line::More(5_000))),
            "the reason with no examples reports all of its files as unnamed"
        );
    }

    #[test]
    fn a_heading_counts_every_file_not_just_the_ones_it_names() {
        let counts = counts_of(&[("Open: Permission denied", 900)]);
        let examples = failed(&[
            ("/t/a", "Open: Permission denied"),
            ("/t/b", "Open: Permission denied"),
        ]);
        let lines = group(&counts, &examples, None);
        assert_eq!(reasons(&lines), vec![("Open: Permission denied".into(), 900)]);
        assert!(matches!(lines.last(), Some(Line::More(898))));
    }

    #[test]
    fn paths_are_shown_relative_to_the_root_that_was_lifted_out() {
        let lines = group_all(
            &failed(&[("/t/one/a.bin", "Open: Permission denied")]),
            Some(Path::new("/t")),
        );
        assert!(
            matches!(&lines[1], Line::File(p) if p == "one/a.bin"),
            "{:?}",
            lines.get(1).map(|_| "not a file line")
        );
    }
}
