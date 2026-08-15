# Owner, group and permission columns, and turning them on by hand.
#
#   FIX=$(scripts/tests/setup-columns.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/columns.sh
#
# The way in is the settings file rather than the menu, and that is the point.
# hoja watches `settings.json` and pushes a re-read over every pane, so writing
# it mid-run covers the whole path a hand-written line takes — serde, the merge
# in `initial_view`, `ColumnLayout::from_flags`, and the header — without a
# single coordinate in the test. The menu is the same `toggle` underneath and
# is checked by eye.
#
# What is *not* asserted: the owner and group text. Whoever ran this owns the
# fixture, and their name comes out of this machine's `/etc/passwd`.

settings="$XDG_CONFIG_HOME/hoja/settings.json"

wait_for 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]'

# --- the set that has always been there --------------------------------------
expect 'p[0]["columns"] == ["Size", "Kind", "Modified"]' \
    "a pane starts with the three columns it always had"

# --- the footer, before any of them are drawn --------------------------------
# The line for a single row says the things no column says. With the usual
# three columns that is the mode and the owner, so it says them.
click "$(named b-script)"
wait_for 'p[0]["selected"] == [p[0]["rows"].index("b-script")]'
expect '"-rwxr-xr-x" in p[0]["footer"]' \
    "the footer carries the mode while no column does"

# --- turning three on by hand ------------------------------------------------
# Named out of table order on purpose: the header follows `Column::ALL`, not
# the order a file happens to list them in.
cat > "$settings" <<'JSON'
{
    "theme": "Rosé Pine",
    "view": { "columns": { "group": true, "permissions": true, "owner": true } }
}
JSON

# The watcher wakes on a 500ms timer and the write lands behind it, so this is
# the one wait in the file that is genuinely waiting for something slow.
wait_for 'p[0]["columns"] == ["Size", "Kind", "Modified", "Permissions", "Owner", "Group"]' 20
echo "  ok   a line in settings.json turns them on, in table order"

# --- what the Permissions column prints --------------------------------------
expect 'p[0]["permissions"][p[0]["rows"].index("b-script")] == "-rwxr-xr-x"' \
    "a file chmod 0755 reads as -rwxr-xr-x"

# The same string, now on the row, so the footer gives it up. All three drawn
# leaves it with nothing to carry at all.
expect 'p[0]["selected"] == [p[0]["rows"].index("b-script")]' \
    "the selection is untouched by a change of columns"
expect 'p[0]["footer"] == ""' \
    "and the footer stops repeating what the columns now show"
expect 'p[0]["permissions"][p[0]["rows"].index("a-folder")] == "drwxr-x---"' \
    "a directory chmod 0750 reads as drwxr-x--- and leads with d"

# --- a list, which sets the order as well as the set -------------------------
# The other shape the block takes. Deliberately not table order, and
# deliberately missing one of the six: a list says exactly what to draw.
cat > "$settings" <<'JSON'
{
    "theme": "Rosé Pine",
    "view": { "columns": ["permissions", "size", "modified"] }
}
JSON
wait_for 'p[0]["columns"] == ["Permissions", "Size", "Modified"]' 20
echo "  ok   a list draws the columns it names, in the order it names them"

# --- and turning one off -----------------------------------------------------
# The three that were always drawn are as hideable as the three that were not,
# which is what makes the submenu one list rather than three fixed rows and
# three offers.
cat > "$settings" <<'JSON'
{
    "theme": "Rosé Pine",
    "view": { "columns": { "size": false, "kind": false, "modified": false } }
}
JSON
wait_for 'p[0]["columns"] == []' 20
echo "  ok   every column can be hidden, leaving the Name column the pane"

# The rows are still there. Hiding every column is a view, not a broken pane.
expect 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]' \
    "and the listing is untouched by any of it"

# --- a header still sorts when it is clicked ---------------------------------
# The hazard the header drag introduces. gpui starts a drag two pixels from
# the mouse down and drops the pending click when it does, so a header that is
# draggable can quietly stop sorting. A click is all `wlrctl` can send — it
# cannot press, move and release, which is why the drag itself is checked by
# hand — but the click is the half that breaks silently.
#
# One column, so its position is arithmetic rather than a guess: Size alone is
# the rightmost 100px of the pane, and the header is the 24px band under the
# toolbar.
cat > "$settings" <<'JSON'
{
    "theme": "Rosé Pine",
    "view": { "columns": ["size"] }
}
JSON
wait_for 'p[0]["columns"] == ["Size"]' 20

at $((WW - 50)) 40
at $((WW - 50)) 40
wait_for 'p[0]["rows"] == ["a-folder", "c-big", "b-script"]' 10
echo "  ok   clicking a header still sorts by it"

# --- the Columns list stays up while it is being used ------------------------
# Six toggles behind one row: dismissing on each of them would mean opening the
# menu once per column. The check marks have to follow, which is the reason the
# rows are rebuilt rather than the menu simply being left alone.
at $((WW - 16)) 14
wait_for 'p[0]["menu_open"]' 20
key -k Down
key -k Down
key -k Down
key -k Right
wait_for 'p[0]["submenu_open"]' 20
expect 'p[0]["submenu_items"] == ["Size", "Kind", "Modified", "Permissions", "Owner", "Group"]' \
    "the Columns list offers all six"

# Opening a submenu lands on its first row, so three Downs from Size reaches
# Permissions, the fourth. Four would be Owner, which is a real answer to a
# different question and looks exactly like an off-by-one in the code.
key -k Down
key -k Down
key -k Down
key -k Return
wait_for 'p[0]["columns"] == ["Size", "Permissions"]' 20
echo "  ok   a toggle in the list shows the column"
expect 'p[0]["submenu_open"]' \
    "and leaves the list open to reach the next one"

key -k Escape
key -k Escape
