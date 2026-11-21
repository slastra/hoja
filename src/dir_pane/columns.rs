//! The list's columns: which there are, how wide, in what order, and the
//! drags that resize and move them.

use super::*;

/// Grab area between two column headers.
pub(super) const COL_HANDLE_WIDTH: f32 = 5.;

pub(super) const COL_MIN_WIDTH: f32 = 56.;

pub(super) const COL_MAX_WIDTH: f32 = 420.;

/// The Name column flexes to fill, but never below this.
pub(super) const NAME_MIN_WIDTH: f32 = 100.;

/// A fixed-width column after the Name column.
///
/// Name is not a variant: it flexes to fill whatever these leave, holds the icon
/// and the rename editor, and has no divider of its own, so it is built directly
/// rather than driven from this table.
///
/// Everything about a column lives here, its order, label, sort key, starting
/// width, and the text it shows, so a new column is one variant, one entry in
/// `ALL`, and one arm in each method. The header, the rows, and the resize
/// handles all follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Size,
    Kind,
    Modified,
    Permissions,
    Owner,
    Group,
}

impl Column {
    /// Display order. The length is part of the type, so adding a variant here
    /// makes the compiler point at `ColumnWidths` rather than silently leaving
    /// the new column without a width.
    /// The three that came first are first, and in the order they have always
    /// been in: their widths are keyed by name, so reordering would cost
    /// nobody anything, but it would move every column of every existing
    /// install for no reason. The three added after them read in the order
    /// `ls -l` prints the same facts.
    pub(super) const ALL: [Column; 6] = [
        Column::Size,
        Column::Kind,
        Column::Modified,
        Column::Permissions,
        Column::Owner,
        Column::Group,
    ];

    /// The key this column's width is stored under. Deliberately not the
    /// display label: renaming what the header says must not discard the width
    /// everyone has already dragged.
    pub(super) fn key(self) -> &'static str {
        match self {
            Column::Size => "size",
            Column::Kind => "kind",
            Column::Modified => "modified",
            Column::Permissions => "permissions",
            Column::Owner => "owner",
            Column::Group => "group",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Column::Size => "Size",
            Column::Kind => "Kind",
            Column::Modified => "Modified",
            Column::Permissions => "Permissions",
            Column::Owner => "Owner",
            Column::Group => "Group",
        }
    }

    pub(super) fn sort_key(self) -> SortKey {
        match self {
            Column::Size => SortKey::Size,
            Column::Kind => SortKey::Kind,
            Column::Modified => SortKey::Modified,
            Column::Permissions => SortKey::Permissions,
            Column::Owner => SortKey::Owner,
            Column::Group => SortKey::Group,
        }
    }

    /// Whether the column's content is held to its right edge.
    ///
    /// Size only, because it is the only column where the digits carry the
    /// meaning: right-aligned, magnitude reads down the column rather than one
    /// row at a time. It aligns the unit, not the decimal point, so a "400 B"
    /// among kilobytes still sits one character right of its neighbours; true
    /// decimal alignment would mean splitting the cell into a right-aligned
    /// number and a left-aligned unit, as the transfer strip does.
    ///
    /// Modified gains nothing from it, since a timestamp is a fixed sixteen
    /// characters and its two edges are the same edge. Kind is words, and so
    /// are Owner and Group. Permissions is ten fixed characters, which is the
    /// Modified case again: both edges are the same edge.
    pub(super) fn aligns_right(self) -> bool {
        matches!(self, Column::Size)
    }

    /// Whether the column prints figures, and so is set in the numeric face.
    ///
    /// Size and Permissions. Kind and Modified are both words now: "Folder",
    /// "BIN", "3 hours ago", and so are Owner and Group. Words in a monospaced
    /// face are just words set badly.
    ///
    /// Permissions is the strongest case of the two. Its ten characters are
    /// ten fixed positions, and a monospaced face is what turns a column of
    /// them into a grid where the execute bit can be read straight down.
    pub(super) fn is_numeric(self) -> bool {
        matches!(self, Column::Size | Column::Permissions)
    }

    /// Wide enough for the widest thing the column prints, at the row's text
    /// size in the numeric face. Monospace is wider than the proportional face
    /// it replaced, so the old 90 and 140 truncated a full timestamp and a
    /// four-digit size the day the font changed. `widest` pins both.
    pub(super) fn default_width(self) -> Pixels {
        match self {
            Column::Size => px(100.),
            Column::Kind => px(90.),
            Column::Modified => px(132.),
            Column::Permissions => px(110.),
            Column::Owner => px(90.),
            Column::Group => px(90.),
        }
    }

    /// How narrow this column may be dragged, and the floor a width restored
    /// from disk is lifted to.
    ///
    /// Per column, because a default is not reachable once anyone has used the
    /// program: `remember_view` writes all three widths on any view change, so
    /// every existing install carries the previous release's numbers, and
    /// raising a default only helps someone installing for the first time. The
    /// Size column widened for the monospaced face and everybody kept 90, which
    /// is two pixels short of "1023.0 MB".
    ///
    /// Kind, Owner and Group keep the bare minimum: their content is words of
    /// no fixed length and eliding them is what the column is for. A user
    /// named from LDAP rather than from `/etc/passwd` shows as a number, which
    /// is shorter still.
    pub(super) fn min_width(self) -> Pixels {
        match self {
            Column::Kind | Column::Owner | Column::Group => px(COL_MIN_WIDTH),
            Column::Size => px(94.),
            Column::Modified => px(124.),
            Column::Permissions => px(102.),
        }
    }

    /// The longest string this column ever holds, for the width test.
    #[cfg(test)]
    pub(super) fn widest(self) -> &'static str {
        match self {
            // `format_size` always prints one decimal and promotes past 1023,
            // so four digits, a point, a decimal and a two-letter unit is the
            // most it can come to.
            Column::Size => "1023.9 MB",
            // Not pinned: "Rust source" and the like come from the file's kind
            // and are already elided when they do not fit.
            Column::Kind => "",
            // The longest `format_time_ago` prints, pinned by its own test.
            Column::Modified => "59 minutes ago",
            // Ten characters, always, whatever the bits are. The sticky bit
            // makes the last one a letter rather than a dash, which is the
            // widest it comes to only because nothing here is wider than
            // anything else.
            Column::Permissions => "drwxrwxrwt",
            // Not pinned, as Kind is not: a name has no fixed length, and an
            // id no database claims prints as its number instead.
            Column::Owner | Column::Group => "",
        }
    }

    /// The cell text for one entry. Empty where the entry has no such value:
    /// unreadable metadata has no time, and a folder has no size until the walk
    /// behind the footer has finished counting it, which is what `folder_bytes`
    /// supplies. Blank until then rather than a figure that climbs: one number
    /// moving at the bottom of the pane reads as progress, a listing full of
    /// them reads as churn.
    pub(super) fn value(
        self,
        entry: &DirEntry,
        folder_bytes: Option<u64>,
        now: SystemTime,
    ) -> String {
        match self {
            Column::Size => entry
                .size
                .or(folder_bytes)
                .map(fs::format_size)
                .unwrap_or_default(),
            Column::Kind => entry.kind(),
            // Read once for the batch of rows being built and passed down,
            // rather than a clock call per row: the answer must be the same for
            // every row in a listing, or two files written in the same second
            // can disagree about how long ago that was.
            Column::Modified => entry
                .modified
                .map(|at| fs::format_time_ago(at, now))
                .unwrap_or_default(),
            // A symlink prints the mode of what it points at, never `l`:
            // `from_std` follows one deliberately, so that a link to a
            // directory sorts and behaves as a directory. Inside an archive
            // this is the one of the three that has an answer, the member
            // header carrying a mode where it carries no owner.
            Column::Permissions => entry.mode.map(fs::format_mode).unwrap_or_default(),
            // The name where the local database has one and the number where
            // it does not, which is a footnote of `owners` rather than of this
            // column. Blank only inside an archive, which names no owner.
            Column::Owner => entry.uid.map(crate::owners::user).unwrap_or_default(),
            Column::Group => entry.gid.map(crate::owners::group).unwrap_or_default(),
        }
    }
}

/// Per-column widths, positional so the set can grow without new fields.
///
/// Indexed by discriminant, not by position in `ALL`, so `ALL` must hold every
/// variant exactly once: otherwise a column indexes a width that belongs to
/// another, or runs off the end.
#[derive(Clone, Copy, Debug)]
pub(super) struct ColumnWidths(pub(super) [Pixels; Column::ALL.len()]);

impl Default for ColumnWidths {
    fn default() -> Self {
        let mut widths = [px(0.); Column::ALL.len()];
        for column in Column::ALL {
            widths[column as usize] = column.default_width();
        }
        Self(widths)
    }
}

/// Which columns this pane draws, and in what order.
///
/// Two positional arrays, and they are positional in opposite directions,
/// which is the whole of it. `shown` is indexed by discriminant like
/// `ColumnWidths`, so `ALL` must hold every variant exactly once. `order` is
/// the inverse: its positions are display slots and its values are the columns
/// in them.
///
/// Holding the order as a permutation of *all* the columns rather than as a
/// list of the visible ones buys three things. `ViewSettings` stays `Copy`,
/// which several call sites rely on. `ColumnWidths` and `shown` keep indexing
/// by discriminant, so neither of them changes. And a hidden column keeps its
/// slot: hide Kind, show it again, and it comes back where it was rather than
/// at the end.
///
/// Lives in `ViewSettings` rather than beside the widths in the workspace
/// state, which is a deliberate difference between the two. A width is a
/// fiddle, remembered so a drag is not lost; a hidden column, or one moved
/// somewhere else, is a preference, worth copying to the pane a split creates
/// and worth writing by hand in `settings.json`. Both come free from being a
/// view setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColumnLayout {
    /// A permutation of `Column::ALL`. Every variant exactly once, or a column
    /// is drawn twice and another not at all.
    pub(super) order: [Column; Column::ALL.len()],
    pub(super) shown: [bool; Column::ALL.len()],
}

impl Default for ColumnLayout {
    /// The table's own order, and exactly the set that existed before any
    /// column could be hidden, so nobody's pane changes under them on an
    /// upgrade.
    fn default() -> Self {
        let mut shown = [false; Column::ALL.len()];
        for column in [Column::Size, Column::Kind, Column::Modified] {
            shown[column as usize] = true;
        }
        Self {
            order: Column::ALL,
            shown,
        }
    }
}

impl ColumnLayout {
    /// What a card shows by default: its size, and nothing else, so a card
    /// stays a picture with a caption.
    pub fn card_default() -> Self {
        let mut shown = [false; Column::ALL.len()];
        shown[Column::Size as usize] = true;
        Self {
            order: Column::ALL,
            shown,
        }
    }

    pub fn get(self, column: Column) -> bool {
        self.shown[column as usize]
    }

    pub(super) fn toggle(&mut self, column: Column) {
        self.shown[column as usize] = !self.shown[column as usize];
    }

    /// The shown columns, in slot order.
    ///
    /// Everything that draws a column goes through this: the header, the
    /// cells, and the resize handle that sits before each one. Yielding
    /// nothing is allowed, and leaves the Name column with the whole pane,
    /// which is a view somebody may well want.
    pub fn iter(self) -> impl Iterator<Item = Column> {
        self.order.into_iter().filter(move |c| self.get(*c))
    }

    /// Where `column` sits among the drawn ones, or `None` if it is not drawn.
    pub(super) fn slot_of(self, column: Column) -> Option<usize> {
        self.iter().position(|c| c == column)
    }

    /// The same layout with `column` moved to `slot`, counting only the drawn
    /// columns.
    ///
    /// Takes and returns a value rather than mutating, because the header drag
    /// recomputes it from the layout it started with on every pointer move.
    /// That is what makes the drag idempotent: the answer depends on where the
    /// pointer started and where it is, never on what the last frame did.
    ///
    /// Hidden columns keep their absolute positions and the drawn ones are
    /// rearranged among the slots that are left, so moving a visible column
    /// never disturbs where an invisible one will reappear.
    pub(super) fn moved(mut self, column: Column, slot: usize) -> Self {
        if !self.get(column) {
            return self;
        }
        let mut drawn: Vec<Column> = self.iter().collect();
        let Some(from) = drawn.iter().position(|c| *c == column) else {
            return self;
        };
        drawn.remove(from);
        drawn.insert(slot.min(drawn.len()), column);

        let mut drawn = drawn.into_iter();
        for column in self.order.iter_mut() {
            if self.shown[*column as usize] {
                *column = drawn.next().expect("as many drawn columns as slots");
            }
        }
        self
    }

    /// The drawn columns in slot order, by name, for the settings and state
    /// files. Keyed by name on the same reasoning as the widths: inserting a
    /// column must not change what an existing file means.
    pub fn names(self) -> Vec<String> {
        self.iter().map(|c| c.key().to_string()).collect()
    }

    /// A layout from an ordered list of names: exactly those columns, in that
    /// order.
    ///
    /// Total, so there is no error to report. Names it does not recognise are
    /// ignored, a name given twice counts once, and anything the list leaves
    /// out is hidden and keeps its place in the table. That last part is what
    /// a list cannot say: a column that is not drawn has no slot to write
    /// down, so one that was moved and then hidden comes back at its table
    /// position rather than where it was left.
    pub fn from_names(names: &[String]) -> Self {
        let mut layout = Self {
            order: Column::ALL,
            shown: [false; Column::ALL.len()],
        };
        let mut slots = Vec::with_capacity(Column::ALL.len());
        for name in names {
            let Some(column) = Column::ALL.into_iter().find(|c| c.key() == name) else {
                continue;
            };
            if layout.shown[column as usize] {
                continue;
            }
            layout.shown[column as usize] = true;
            slots.push(column);
        }
        // Whatever was not named, in the order the table has them, so the
        // hidden columns sit in a defined place rather than wherever a `Vec`
        // left them.
        slots.extend(
            Column::ALL
                .into_iter()
                .filter(|c| !layout.shown[*c as usize]),
        );
        layout.order = slots.try_into().expect("every column, exactly once");
        layout
    }

    /// A layout from a map of adjustments: the usual set and the usual order,
    /// with only the columns named changed.
    ///
    /// What a hand-written `{"permissions": true}` means. Unknown keys are
    /// ignored, so a column that no longer exists cannot resurrect itself.
    ///
    /// `base` is the usual set: `default()` for the columns, `card_default()`
    /// for the card details.
    pub fn from_flags(base: Self, flags: &std::collections::HashMap<String, bool>) -> Self {
        let mut layout = base;
        for column in Column::ALL {
            if let Some(on) = flags.get(column.key()) {
                layout.shown[column as usize] = *on;
            }
        }
        layout
    }
}

impl ColumnWidths {
    pub(super) fn get(&self, column: Column) -> Pixels {
        self.0[column as usize]
    }

    pub(super) fn set(&mut self, column: Column, width: Pixels) {
        self.0[column as usize] = width.clamp(column.min_width(), px(COL_MAX_WIDTH));
    }
}

/// What `on_drag_move` dispatches on, and who started it.
///
/// The type alone is not enough. gpui routes a drag-move to every handler
/// registered for the payload's *type*, so a resize in one pane is delivered to
/// the identically-typed handlers in every other pane as well. Carrying the
/// pane and the column means each handler can tell whether the drag is its own,
/// which the pane's own `resize` anchor cannot: that is set on mouse down and
/// never cleared, so a pane that was ever resized keeps an anchor from a drag
/// that ended long ago. A second pane then answered someone else's drag with a
/// start position hundreds of pixels away and drove its column straight into
/// `COL_MIN_WIDTH`.
#[derive(Clone, Copy)]
pub(super) struct ColumnResize {
    pub(super) pane: gpui::EntityId,
    pub(super) column: Column,
}

/// What a header reorder dispatches on, and who started it.
///
/// The same shape as `ColumnResize` and for the same reason: gpui routes a
/// drag-move to every handler registered for the payload's type, in every
/// pane, so the payload has to say whose drag it is.
#[derive(Clone, Copy)]
pub(super) struct ColumnMove {
    pub(super) pane: gpui::EntityId,
    pub(super) column: Column,
}

/// Where a header reorder started, captured once when the drag begins.
///
/// Everything here is frozen at the start, and that is the design rather than
/// an economy. The target slot is recomputed from these on every pointer move,
/// so the drag is a pure function of where it began and where the pointer is:
/// nothing accumulates, nothing drifts, and there is no state to unwind when it
/// ends. Recomputing from the *live* layout instead would feed the result back
/// into its own input, which is the oscillation `on_drag_move`'s stale bounds
/// are already known for.
pub(super) struct ColumnMoveDrag {
    pub(super) column: Column,
    pub(super) start_x: Pixels,
    /// Its slot among the drawn columns, and the distance from each slot to
    /// the next: a column's width *plus* the resize handle in front of it,
    /// because that is what a swap actually displaces the header by. Widths
    /// alone fire the swap early and drift further with every column crossed.
    pub(super) start_slot: usize,
    pub(super) start_pitches: Vec<f32>,
    pub(super) start_layout: ColumnLayout,
    /// Whether the pointer moved at all while the button was down, and how far
    /// it had gone when it last did. What tells a drag from a click, and a
    /// press that wandered from one that did not; see the mouse-up handler.
    pub(super) dragged: bool,
    pub(super) moved_by: f32,
}

/// How far a press may wander and still be a click on a header.
///
/// gpui promotes a press to a drag two pixels out (`DRAG_THRESHOLD`), which is
/// well inside the hand movement in an ordinary click, so a header that can be
/// dragged needs its own idea of how still a click has to be. Comfortably
/// larger than gpui's number and far smaller than half a column, which is what
/// it takes to actually move one.
pub(super) const HEADER_CLICK_SLOP: f32 = 5.;

/// Which slot a header being dragged has reached.
///
/// `pitches` is how far apart the drawn columns are in slot order, `from` is
/// the slot the drag started in, and `dx` is how far the pointer has travelled
/// since. Advances a slot each time `|dx|` passes the pitches already crossed
/// plus half of the next one, which is the dragged column's centre passing its
/// neighbour's.
///
/// A pitch is the column's width plus the handle in front of it, since the
/// header lays out as `[handle][head][handle][head]`. That five pixels is the
/// difference between the swap landing where the eye expects it and landing
/// slightly early, every time, cumulatively.
///
/// Arithmetic rather than a hit test against the headers, deliberately.
/// `on_drag_move` sees the bounds of the last painted frame and a reorder
/// changes exactly those bounds, so asking "am I over my neighbour now"
/// answers a question about a layout that has already been replaced. This asks
/// only where the pointer started and where it is, which no repaint can move.
pub(super) fn target_slot(pitches: &[f32], from: usize, dx: f32) -> usize {
    let mut slot = from;
    let mut crossed = 0.;
    if dx > 0. {
        for (ix, pitch) in pitches.iter().enumerate().skip(from + 1) {
            if dx < crossed + pitch / 2. {
                break;
            }
            crossed += pitch;
            slot = ix;
        }
    } else {
        for (ix, pitch) in pitches.iter().enumerate().take(from).rev() {
            if -dx < crossed + pitch / 2. {
                break;
            }
            crossed += pitch;
            slot = ix;
        }
    }
    slot
}

/// Where a column resize started, captured once when the drag begins.
#[derive(Clone, Copy)]
pub(super) struct ColumnDrag {
    pub(super) column: Column,
    pub(super) start_x: Pixels,
    pub(super) start_width: Pixels,
}

/// One fixed-width cell of a listing row, resolved before the element is built.
///
/// A struct rather than a tuple because the call site was passing five
/// positional arguments, two of them bare bools, and `cell(w, t, n, true, false)`
/// says nothing about which of those is the alignment.
pub(super) struct Cell {
    pub(super) width: Pixels,
    pub(super) text: String,
    /// The monospaced family, for a column that prints figures.
    pub(super) numeric: Option<gpui::SharedString>,
    /// Show the breathing bar rather than `text`, which is empty anyway while
    /// the size behind it is still being counted.
    pub(super) counting: bool,
    pub(super) right: bool,
    /// Part of the column being dragged to a new slot, and tinted to say so.
    pub(super) carried: bool,
}

/// Drags need a preview view; column resizing shows nothing.
pub(super) struct EmptyDrag;

impl Render for EmptyDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
