# Owner, group and permission columns, and turning them on by hand.
#
#   FIX=$(scripts/tests/setup-columns.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/columns.sh
#
# The way in is the settings file rather than the menu, and that is the point.
# hoja watches `settings.json` and pushes a re-read over every pane, so writing
# it mid-run covers the whole path a hand-written line takes — serde, the merge
# in `initial_view`, `ColumnsShown::from_map`, and the header — without a
# single coordinate in the test. The menu is the same `toggle` underneath and
# is checked by eye.
#
# What is *not* asserted: the owner and group text. Whoever ran this owns the
# fixture, and their name comes out of this machine's `/etc/passwd`.

settings="$XDG_CONFIG_HOME/hoja/settings.json"

wait_for 'p[0]["rows"] == ["a-folder", "b-script"]'

# --- the set that has always been there --------------------------------------
expect 'p[0]["columns"] == ["Size", "Kind", "Modified"]' \
    "a pane starts with the three columns it always had"

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
expect 'p[0]["permissions"][p[0]["rows"].index("a-folder")] == "drwxr-x---"' \
    "a directory chmod 0750 reads as drwxr-x--- and leads with d"

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
expect 'p[0]["rows"] == ["a-folder", "b-script"]' \
    "and the listing is untouched by any of it"
