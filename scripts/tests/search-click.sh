# Clicking a result keeps the results.
#
#   FIX=$(scripts/tests/setup-columns.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/search-click.sh
#
# The search field emitted the same event when focus left it as when Escape was
# pressed, and the pane read both as "give up". So clicking a result blurred the
# field, ended the search, and put the folder listing back — taking away the
# thing the click was reaching for. Opening anything found by searching meant
# not touching it with the mouse.
#
# Runs against the columns fixture, which has a folder, a file and a third row,
# because any directory with something under it will do.

wait_for 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]'

# --- search, and land on results ---------------------------------------------
key -M ctrl -P f -m ctrl
sleep 0.3
# "scr" rather than "b": matching is a substring anywhere in the name, and "b"
# also picks out `c-big`, which makes the row a click lands on a guess.
key s
key c
key r
wait_for 'p[0]["searching"]' 20
wait_for 'p[0]["rows"] == ["b-script"]' 20
expect 'p[0]["searching"]' "ctrl-f with a query shows what matches"

# --- the click that used to end it -------------------------------------------
click 0
sleep 0.5
expect 'p[0]["searching"]' "clicking a result leaves the search standing"
expect 'p[0]["rows"] == ["b-script"]' "and the results are still the rows"
expect 'p[0]["selected"] == [0]' "and the row a click landed on is selected"

# --- the arrows reach the results without leaving the field ------------------
# Every pane binding is masked while a field has the keyboard, because on Linux
# the keymap is consulted before text input and a letter would otherwise fire
# whatever it is bound to. The arrows type nothing, so the mask cost more than
# it saved: the results were on screen with the first one already current, and
# moving off it meant leaving the field first.
key -M ctrl -P f -m ctrl
sleep 0.3
key -k BackSpace
key -k BackSpace
key -k BackSpace
key c
wait_for 'len(p[0]["rows"]) == 2' 20
expect 'p[0]["cursor"] == 0' "results arrive with the first one current"

key -k Down
sleep 0.4
expect 'p[0]["cursor"] == 1' "down moves to the next result"
key -k Up
sleep 0.4
expect 'p[0]["cursor"] == 0' "and up moves back"

# The field still has the keyboard, so the query can still be edited. This is
# the half that says the arrows were given back rather than the focus moved.
key -k BackSpace
wait_for 'len(p[0]["rows"]) == 3' 20
echo "  ok   and the query is still being typed into"

key -k Escape
sleep 0.4

# --- Escape still means Escape -----------------------------------------------
# The distinction this rests on: giving up is deliberate, losing focus is not.
key -k Escape
wait_for 'not p[0]["searching"]' 20
expect 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]' \
    "and Escape still puts the folder back"
