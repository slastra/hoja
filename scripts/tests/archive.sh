# Browsing a zip in a pane. Run against the fixture directory that
# `scripts/tests/setup-archives.sh` builds:
#
#   FIX=$(scripts/tests/setup-archives.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/archive.sh

wait_for 'p[0]["row_count"] == 9'
expect 'p[0]["rows"][0] == "bare.zip"' "the fixture lists"

# --- stepping in --------------------------------------------------------
dbl "$(named fonts.zip)"
wait_for 'p[0]["dir"].endswith("/fonts.zip")'
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
expect 'p[0]["rows"] == ["ttf", "LICENSE"]' "the archive root lists, folders first"
expect 'p[0]["error"] is None' "a readable zip is not an error"

# The whole reason for the design: a folder inside an archive knows its own
# total exactly, so the Size column is filled the moment the listing lands and
# nothing is ever left counting.
expect 'p[0]["counting"] == []' "no folder in an archive is left counting"
expect 'p[0]["sizes"][0] == "6.0 KB"' "the folder totals its subtree"
expect 'p[0]["footer"] == "2 items · 6.1 KB"' "the footer is final at once"

# --- searching inside an archive -----------------------------------------
# `ctrl-f` searches every member below this directory, the same as it does in
# a real one. There is nothing to walk: the arrangement the read built knows
# every member, so this is a pass over memory with no thread and no debounce.
#
# `ttf` matches at three depths — the folder, a file in it, and a file one
# further down — which is what proves the search recurses rather than
# filtering the rows on screen. `sub` is not a match: names are matched, not
# paths, so a query does not drag in everything under a folder it happens to
# name.
key -M ctrl -P f -m ctrl
# Settled rather than waited for: the field takes focus a frame after the key,
# and there is nothing in the probe that says so — `searching` only becomes
# true once there is a query, which is the thing being typed. Without this the
# text can land on the listing instead, leaving an empty query that matches
# everything and reads as a search that found too much.
sleep 0.3
key ttf
wait_for 'p[0]["rows"] == ["ttf", "ttf/Inter.ttf", "ttf/sub/Mono.ttf"]'
expect 'p[0]["rows"] == ["ttf", "ttf/Inter.ttf", "ttf/sub/Mono.ttf"]' \
    "ctrl-f finds members at every depth, labelled by where they sit"
expect 'p[0]["footer"] == "3 matches"' "and the footer counts them"
expect 'p[0]["counting"] == []' "a found folder already knows its own total"

# One hit reads as one, not "1 matches".
key -k BackSpace
key -k BackSpace
key -k BackSpace
key Mono
wait_for 'p[0]["rows"] == ["ttf/sub/Mono.ttf"]'
expect 'p[0]["footer"] == "1 match"' "a single hit is singular"

key -k Escape
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
expect 'p[0]["rows"] == ["ttf", "LICENSE"]' "escape puts the full listing back"
expect 'p[0]["footer"] == "2 items · 6.1 KB"' "with the ordinary footer, not stuck on the search"

dbl "$(named ttf)"
wait_for 'p[0]["dir"].endswith("/fonts.zip/ttf")'
wait_for 'p[0]["rows"] == ["sub", "Inter.ttf"]'
expect 'p[0]["rows"] == ["sub", "Inter.ttf"]' "a folder inside the archive opens"
expect 'p[0]["sizes"][0] == "2.0 KB"' "and so does its own subtree total"

# --- refusals -----------------------------------------------------------
key -k Down
wait_for 'p[0]["cursor"] == 0'
key -M shift -P Down -m shift
wait_for 'p[0]["selected"] == [0, 1]'

key -k Delete
wait_for 'd["notice"] is not None'
expect '"delete" in d["notice"]' "delete is refused, not attempted"
expect 'p[0]["row_count"] == 2' "and nothing left the listing"

# Enter on a file asks before extracting a temp copy to open, rather than
# either refusing outright or doing it without asking. See `open_prompt`.
before=$(find /tmp -maxdepth 1 -name 'hoja-open-*' 2>/dev/null | wc -l)

key -k Down
wait_for 'p[0]["cursor"] == 1'
key -k Return
wait_for 'd["modal"] == "open-prompt"'
expect 'p[0]["dir"].endswith("/fonts.zip/ttf")' "and the pane stayed put"

# Cancelling leaves nothing extracted.
key -k Escape
wait_for 'd["modal"] is None'
after_cancel=$(find /tmp -maxdepth 1 -name 'hoja-open-*' 2>/dev/null | wc -l)
if [ "$after_cancel" -eq "$before" ]; then
    echo "  ok   cancelling the prompt extracts nothing"
else
    echo "  FAIL cancelling the prompt extracts nothing" >&2
    FAILED=$((FAILED + 1))
fi

# Confirming extracts the one file into a fresh temp directory.
key -k Return
wait_for 'd["modal"] == "open-prompt"'
key -k Return
wait_for 'd["modal"] is None'
extracted=$(ls -td /tmp/hoja-open-* 2>/dev/null | head -1)
if [ -n "$extracted" ] && [ -f "$extracted/Inter.ttf" ]; then
    echo "  ok   confirming extracts the file to a temp copy"
else
    echo "  FAIL confirming extracts the file to a temp copy" >&2
    FAILED=$((FAILED + 1))
fi
rm -rf "$extracted"

# --- getting out again --------------------------------------------------
key -k BackSpace
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
key -k BackSpace
wait_for 'p[0]["rows"][0] == "bare.zip"'
expect 'p[0]["row_count"] == 9' "up from the archive root lands beside it"

# --- an archive with no directory entries -------------------------------
# Three of twelve real zip files hold none, so this listing has to match the
# one above rather than being a special case.
dbl "$(named bare.zip)"
wait_for 'p[0]["dir"].endswith("/bare.zip")'
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
expect 'p[0]["rows"] == ["ttf", "LICENSE"]' "folders it never named are still listed"
expect 'p[0]["sizes"][0] == "6.0 KB"' "and still total correctly"

key -k BackSpace
wait_for 'p[0]["rows"][0] == "bare.zip"'

# --- a file that is not a zip -------------------------------------------
# Five of 127 were like this. It has to say so and stay where it is, not bounce
# to the parent the way a missing directory does.
dbl "$(named broken.zip)"
wait_for 'p[0]["error"] is not None'
expect '"not a zip" in p[0]["error"]' "a broken archive says what is wrong"
expect 'p[0]["dir"].endswith("/broken.zip")' "and does not climb out on its own"

key -k BackSpace
wait_for 'p[0]["rows"][0] == "bare.zip"'

# --- copying out --------------------------------------------------------
# A folder, so this covers the part that has no member of its own to look up:
# an archive that names no folders still has to hand over the whole of one.
dbl "$(named fonts.zip)"
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
key -k Down
wait_for 'p[0]["cursor"] == 0'
expect 'p[0]["rows"][0] == "ttf"' "the folder is the one selected"
key -M ctrl -P c -m ctrl

key -k BackSpace
wait_for 'p[0]["rows"][0] == "bare.zip"'
key -M ctrl -P v -m ctrl

# The listing gains it once the extraction and the move behind it are done.
wait_for '"ttf" in p[0]["rows"]' 20
expect 'p[0]["row_count"] == 10' "the folder arrives beside the archive"
wait_for 'p[0]["counting"] == []' 15
expect 'p[0]["sizes"][p[0]["rows"].index("ttf")] == "6.0 KB"' "with everything that was under it"

# The staging directory is named the way a part-written file is, so it is
# hidden while it exists and gone afterwards either way.
expect 'not any(r.startswith(".hoja-") for r in p[0]["rows"])' "and nothing is left staged"

# --- an empty context menu stays closed -----------------------------------
# Nothing here can be renamed, deleted, cut, pasted, or turned into a new
# folder, and empty space has no row to offer Copy on either, so there is
# nothing at all to put in a menu. It should not open rather than open empty.
dbl "$(named fonts.zip)"
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
right 5
expect 'not p[0]["menu_open"]' "right-click on empty archive space opens no menu"

# A row still gets one, which is what proves the right-click above actually
# landed rather than the check passing by accident.
right "$(named ttf)"
wait_for 'p[0]["menu_open"]'
key -k Escape
wait_for 'not p[0]["menu_open"]'

# Back to the fixture root as it stood after "copying out" above: the "ttf"
# pasted there sorts first now, folders before files, not "bare.zip".
key -k BackSpace
wait_for 'p[0]["row_count"] == 10'

# --- copying out a search result ------------------------------------------
# The claim this pins: a search result copies out correctly with no change to
# extraction at all. A row carries the archive's path with the member's on the
# end, `selected_in_archive` strips the archive back off, and `extract` strips
# the directory the row was found *from* — so a hit two levels down lands with
# the structure below that directory and nothing above it.
dbl "$(named fonts.zip)"
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
dbl "$(named ttf)"
wait_for 'p[0]["dir"].endswith("/fonts.zip/ttf")'

key -M ctrl -P f -m ctrl
sleep 0.3
key Mono
wait_for 'p[0]["rows"] == ["sub/Mono.ttf"]'
expect 'p[0]["rows"] == ["sub/Mono.ttf"]' "the nested hit is labelled from the directory searched"
# Enter keeps the results and gives the listing back the keyboard. Without it
# the arrow and the copy below go to the search field, where ctrl-c copies
# text rather than files and the paste later does nothing at all.
key -k Return
key -k Down
wait_for 'p[0]["cursor"] == 0'
key -M ctrl -P c -m ctrl
key -k Escape

key -k BackSpace
key -k BackSpace
wait_for 'p[0]["row_count"] == 10'
key -M ctrl -P v -m ctrl

wait_for '"sub" in p[0]["rows"]' 20
expect 'p[0]["row_count"] == 11' "the folder it was found in arrives, not the path down to it"
if [ -f "$START_DIR/sub/Mono.ttf" ]; then
    echo "  ok   and the member itself landed inside it"
else
    echo "  FAIL and the member itself landed inside it" >&2
    echo "       $(find "$START_DIR/sub" 2>&1 | head -3)" >&2
    FAILED=$((FAILED + 1))
fi
expect 'not any(r.startswith(".hoja-") for r in p[0]["rows"])' "with nothing left staged"
