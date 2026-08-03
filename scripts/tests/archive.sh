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
# There is no directory to walk, but the whole listing is already in memory,
# so `ctrl-f` filters it in place: no walk, no debounce, and no way to end up
# staring at a listing that stays empty forever the way an early version of
# this did.
key -M ctrl -P f -m ctrl
key ttf
wait_for 'p[0]["rows"] == ["ttf"]'
expect 'p[0]["rows"] == ["ttf"]' "ctrl-f filters the archive listing in place"
expect 'p[0]["footer"] == "1 match"' "and the footer counts the match"
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
