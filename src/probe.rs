//! What the window is showing, in a form a test can assert on.
//!
//! Off unless `HOJA_PROBE` names a file, so this costs a released build one
//! `Option` check per frame and nothing else.
//!
//! It exists because every interface test used to end in a screenshot that a
//! person had to look at. That is slow, it cannot run unattended, and most of
//! what was being checked was never a picture in the first place: which rows
//! are listed, what is selected, what the footer totals, whether the walk has
//! settled. The program knows all of that exactly. Reading it back out of a
//! rendering was answering a question the program could just answer.
//!
//! Screenshots keep the half they are actually for, which is whether the thing
//! *looks* right: alignment, colour, spacing, whether a bar is pulsing.
//!
//! # The file
//!
//! One JSON object, rewritten whenever it would differ. A reader can poll it
//! and know that what it holds is what is on screen, which is what lets a test
//! wait for a condition rather than sleep and hope.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Rows carried per pane.
///
/// Bounded because a directory can hold any number of them and this is written
/// from `render`: serializing a hundred thousand rows would make the program
/// slower in exactly the case a test is most likely to be measuring. Nothing
/// asserts on the ten-thousandth row anyway. `row_count` stays exact, so a test
/// that cares how many there are still gets the truth.
pub const MAX_ROWS: usize = 200;

/// A pane, as a test sees it.
#[derive(Serialize, PartialEq, Eq)]
pub struct PaneProbe {
    pub dir: PathBuf,
    pub active: bool,
    /// Every row, however many are carried below.
    pub row_count: usize,
    /// Row labels in display order, which is what an assertion names. The first
    /// `MAX_ROWS` of them.
    pub rows: Vec<String>,
    /// The Size column as printed, so a test checks the text a person reads
    /// rather than the bytes behind it. Empty where a cell is blank, which is
    /// how "still counting" reads.
    pub sizes: Vec<String>,
    pub modified: Vec<String>,
    pub selected: Vec<usize>,
    pub cursor: Option<usize>,
    pub footer: String,
    /// Rows whose folder size the walk has not finished, by index.
    pub counting: Vec<usize>,
    pub searching: bool,
    pub error: Option<String>,
}

/// A transfer, as a test sees it.
#[derive(Serialize, PartialEq, Eq)]
pub struct JobProbe {
    pub label: String,
    pub done: bool,
    pub errors: usize,
}

#[derive(Serialize, PartialEq, Eq)]
pub struct Probe {
    pub panes: Vec<PaneProbe>,
    pub jobs: Vec<JobProbe>,
    /// `"palette"`, `"places"`, `"failures"`, or absent.
    pub modal: Option<&'static str>,
    pub notice: Option<String>,
    /// Bumped on every write, so a reader can tell a fresh answer from the same
    /// one twice without comparing the whole object. Set by `write`, which is
    /// the only thing that knows whether a write happened; a caller filling it
    /// in would make every frame differ from the last and defeat the check
    /// below.
    pub revision: u64,
}

/// Where to write, read once. `None` in every build nobody asked to probe.
pub fn path() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("HOJA_PROBE").map(PathBuf::from))
        .as_deref()
}

/// Write `probe` if it says something the last one did not.
///
/// Compared before writing because `render` runs on every frame and an
/// animation repaints without changing anything: the counting bar alone would
/// otherwise rewrite this file sixty times a second while saying the same thing.
pub fn write(probe: &mut Probe) {
    let Some(path) = path() else { return };
    // Serialized with the revision still zero, so what is compared is what the
    // window is showing and not a counter that changes every time it is read.
    probe.revision = 0;
    let Ok(body) = serde_json_lenient::to_string_pretty(&*probe) else {
        return;
    };

    static LAST: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    let Ok(mut last) = LAST.lock() else { return };
    if *last == body {
        return;
    }

    static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    probe.revision = REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Ok(full) = serde_json_lenient::to_string_pretty(&*probe) else {
        return;
    };

    // Written whole and renamed over, so a reader never sees half a document.
    // The reader polls this file; a partial read would be a test failing on a
    // parse error rather than on what it was checking.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &full).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        // The revision-free form, since that is what the next frame compares.
        *last = body;
    }
}
