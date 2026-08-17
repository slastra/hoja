# Using search results with the mouse and the arrows.
#
#   FIX=$(scripts/tests/setup-columns.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/search-click.sh
#
# Two things that were both the same mistake. The search field emitted one
# event for "Escape" and "focus left", and the pane read both as give up, so
# clicking a result ended the search and put the folder back — taking away what
# the click was reaching for. And every pane binding is masked while a field
# has the keyboard, because on Linux the keymap is consulted before text input
# and a letter would fire whatever it is bound to; that mask is all or nothing
# and took the arrows with it.
#
# Runs against the columns fixture: any directory with a few things in it.

wait_for 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]'

# --- results ------------------------------------------------------------------
# "c" is in both `b-script` and `c-big`, so there are two of them and which
# comes first is discovery order rather than anything worth asserting.
key -M ctrl -P f -m ctrl
sleep 0.3
key c
wait_for 'len(p[0]["rows"]) == 2' 20
expect 'p[0]["searching"]' "ctrl-f with a query shows what matches"
expect 'p[0]["cursor"] == 0' "and makes the first of them current"

# --- clicking one keeps them --------------------------------------------------
# The *second* result. The first is already current the moment results arrive,
# so clicking that one would assert the same thing whether the click landed or
# not. The cursor moving is what says it landed.
click 1
wait_for 'p[0]["cursor"] == 1' 10
expect 'p[0]["searching"]' "clicking a result leaves the search standing"
expect 'len(p[0]["rows"]) == 2' "and the results are still the rows"
expect 'p[0]["selected"] == [1]' "and the row clicked is the one selected"

# --- the arrows reach them without leaving the field --------------------------
# The click closed the field, so this opens it again; the query it was holding
# comes back with it.
key -M ctrl -P f -m ctrl
wait_for 'len(p[0]["rows"]) == 2' 20
key -k Up
wait_for 'p[0]["cursor"] == 0' 10
echo "  ok   up moves through the results from inside the field"
key -k Down
wait_for 'p[0]["cursor"] == 1' 10
echo "  ok   and so does down"

# Still typing into the field, which is what says the arrows were given back
# rather than the focus moved. "cr" narrows to `b-script` alone and leaves the
# search running, where a backspace would empty the query and end it.
#
# End first: reopening the field selects the query it was holding, so a letter
# typed straight away replaces it instead of adding to it.
key -k End
key r
wait_for 'p[0]["rows"] == ["b-script"]' 20
echo "  ok   and the query is still being typed into"

# --- Escape still means Escape ------------------------------------------------
# Deliberately giving up, against a search that is still running: the whole
# point of splitting the two events is that this still works.
expect 'p[0]["searching"]' "the search is live going into this"
key -k Escape
wait_for 'not p[0]["searching"]' 20
expect 'p[0]["rows"] == ["a-folder", "b-script", "c-big"]' \
    "and Escape puts the folder back"
