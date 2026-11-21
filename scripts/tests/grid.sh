# The grid layout: cards dealt into columns in sort order, pictures loading
# into them, and the keyboard and pointer working as they do in the list.
#
#   FIX=$(scripts/tests/setup-grid.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/grid.sh
#
# Run it nested: it types, and headless mode drops keys.

wait_for 'p[0]["row_count"] == 44'
expect 'p[0]["layout"] == "list"' "a pane starts as a list"

# --- switching ---------------------------------------------------------------
key -M ctrl -k 2 -m ctrl
wait_for 'p[0]["layout"] == "grid"'
expect 'p[0]["grid_columns"] >= 2' "ctrl-2 shows a grid of several columns"
N=$(probe 'p[0]["grid_columns"]')
expect 'p[0]["cards"][0][0] == 0 and p[0]["cards"][0][2] == p[0]["cards"][1][2]' \
    "the first cards start a band together"
wait_for 'sum(c[5] for c in p[0]["cards"]) >= 10' 30
expect 'sum(c[5] for c in p[0]["cards"]) >= 10' "pictures arrive in the cards"
shot grid

# --- the keyboard ------------------------------------------------------------
card 3
wait_for 'p[0]["cursor"] == 3'
expect 'p[0]["selected"] == [3]' "a click selects the card"
key -k Right
wait_for 'p[0]["cursor"] == 4'
expect 'p[0]["selected"] == [4]' "right moves to the next card"
key -k Down
wait_for "p[0]['cursor'] == 4 + $N"
expect "p[0]['cursor'] == 4 + $N" "down moves a band, in the same column"
key -k Left
wait_for "p[0]['cursor'] == 3 + $N"
key -k Up
wait_for 'p[0]["cursor"] == 3'
expect 'p[0]["cursor"] == 3' "left and up come back"
key -M shift -k Right -m shift
wait_for 'p[0]["selected"] == [3, 4]'
expect 'p[0]["selected"] == [3, 4]' "shift-right extends the selection"

key -k End
wait_for 'p[0]["cursor"] == 43'
wait_for 'p[0]["top_row"] > 0'
expect 'any(c[0] == 43 for c in p[0]["cards"])' "end scrolls to the last card"
shot grid-end
key -k Home
wait_for 'p[0]["cursor"] == 0 and p[0]["top_row"] == 0'
expect 'p[0]["top_row"] == 0' "home scrolls back to the top"

# --- the pointer -------------------------------------------------------------
rcard 5
wait_for 'p[0]["menu_open"]'
expect 'p[0]["selected"] == [5]' "a right-click selects the card and opens the menu"
key -k Escape
wait_for 'not p[0]["menu_open"]'

ALBUMS=$(named albums)
dcard "$ALBUMS"
wait_for 'p[0]["dir"].endswith("/albums")'
expect 'p[0]["layout"] == "grid"' "a double-click opens a folder, still as a grid"
key -M alt -k Left -m alt
wait_for 'p[0]["row_count"] == 44'
expect "p[0]['cursor'] == $ALBUMS" "back returns to the folder that was opened"

# --- and back ----------------------------------------------------------------
key -M ctrl -k 1 -m ctrl
wait_for 'p[0]["layout"] == "list"'
expect 'p[0]["cards"] == [] and p[0]["grid_columns"] == 0' "ctrl-1 returns to the list"
