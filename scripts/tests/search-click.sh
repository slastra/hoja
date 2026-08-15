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

# --- Escape still means Escape -----------------------------------------------
# The distinction this rests on: giving up is deliberate, losing focus is not.
key -k Escape
wait_for 'not p[0]["searching"]' 20
expect 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]' \
    "and Escape still puts the folder back"
