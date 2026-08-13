# "Open With" as a submenu.
#
#   FIX=$(scripts/tests/setup-context-menu.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/context-menu.sh
#
# Keyboard after the right-click. The submenu opens on hover too, but hovering
# means deriving a row's y from menu chrome that has no probe to check it
# against, and being one row out opens nothing and looks exactly like a
# submenu that does not work. Right and Left are the same code path with no
# coordinates in it.
#
# The applications are whatever this machine registers for text/plain, so
# nothing here asserts a name — only that there are some, that they are behind
# one row rather than spilled into the menu, and that more than eight survive
# where the old flat list stopped.

wait_for 'p[0]["rows"] == ["a-folder", "shot.png"]'

# --- the submenu exists, and the applications are inside it ------------------
right 1
wait_for 'p[0]["menu_open"]' 20
expect '"Open" in p[0]["menu_items"]' "right-clicking a file offers Open"

# Resolved off the UI thread, so it arrives a moment after the menu itself.
wait_for '"Open With" in p[0]["menu_items"]' 30
expect 'not p[0]["submenu_open"]' "and its submenu starts closed"

# Nothing named "Open with X" is left loose in the menu: that was the flat list.
expect 'not any(i.startswith("Open with ") for i in p[0]["menu_items"])' \
    "the applications are not spilled into the menu"

# --- opening it --------------------------------------------------------------
# Two Downs: "Open" is the first row and "Open With" is the second.
key -k Down
key -k Down
key -k Right
wait_for 'p[0]["submenu_open"]' 20
expect 'len(p[0]["submenu_items"]) > 0' "the submenu holds applications"

# The cap the flat list needed is gone.
#
# Three outcomes, because the number depends on what is installed. More than
# eight proves it. Fewer than eight means this machine simply has that few
# handlers and the question cannot be asked. *Exactly* eight is the old cap's
# fingerprint and is treated as a failure: a desktop landing on that number by
# coincidence is possible but far less likely than `.take(8)` coming back.
count=$(probe 'len(p[0]["submenu_items"])')
if [ "$count" -gt 8 ]; then
    echo "  ok   more than the old cap of 8 survives ($count applications)"
elif [ "$count" -eq 8 ]; then
    echo "  FAIL exactly 8 applications, which is what the old cap allowed" >&2
    echo "       either .take(8) is back, or this machine has exactly 8 PNG" >&2
    echo "       handlers; check with: gio mime image/png" >&2
    FAILED=$((FAILED + 1))
else
    echo "  ..   only $count applications registered here; the cap is untestable"
fi

# --- backing out of it -------------------------------------------------------
key -k Left
wait_for 'not p[0]["submenu_open"]' 20
expect 'p[0]["menu_open"]' "Left closes the submenu and keeps the menu"

key -k Right
wait_for 'p[0]["submenu_open"]' 20
key -k Escape
wait_for 'not p[0]["submenu_open"]' 20
expect 'p[0]["menu_open"]' "and Escape closes the submenu before the menu"

key -k Escape
wait_for 'not p[0]["menu_open"]' 20

# --- a folder has nothing to open with ---------------------------------------
right 0
wait_for 'p[0]["menu_open"]' 20
expect '"Open With" not in p[0]["menu_items"]' \
    "a folder offers no applications"
key -k Escape
wait_for 'not p[0]["menu_open"]' 20
