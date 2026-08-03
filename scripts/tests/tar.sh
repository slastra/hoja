# Tarballs in a pane, in every codec.
#
#   FIX=$(scripts/tests/setup-archives.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/tar.sh
#
# The fixture holds the same tree five times over, so the interesting assertion
# is that all five list identically: the codec is only which reader the file is
# wrapped in, and nothing downstream should be able to tell them apart.
#
# Every step back out waits for the fixture directory *by name* rather than for
# a row count. `backspace` walks up without a floor, and a script that has lost
# track of where it is will happily keep walking to the root of the filesystem
# and then paste there. `wait_for` now ends the run rather than carrying on, and
# naming the directory is what gives it something real to end on.

wait_for 'p[0]["row_count"] == 9'
expect 'p[0]["rows"][0] == "bare.zip"' "the fixture lists"

for name in fonts.tar fonts.tar.bz2 fonts.tar.gz fonts.tar.xz fonts.tar.zst; do
    dbl "$(named "$name")"
    wait_for "p[0][\"dir\"].endswith(\"/$name\")" 30
    # The rows themselves, not `not reading`: that one is true before the read
    # starts as well as after it ends, so it passes the instant the pane
    # navigates and everything below then races the listing.
    wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]' 30
    expect 'not p[0]["reading"]' "$name finishes reading"
    expect 'p[0]["rows"] == ["ttf", "LICENSE"]' "$name lists its root"
    expect 'p[0]["error"] is None' "$name is not an error"
    # Every codec, every time: the folder total is exact the moment it lands,
    # because the archive already said what is in it.
    expect 'p[0]["counting"] == []' "$name leaves nothing counting"
    expect 'p[0]["footer"] == "2 items · 14 B"' "$name totals the same"

    key -k BackSpace
    wait_for "p[0][\"dir\"] == \"$START_DIR\""
done

# --- inside one of them ------------------------------------------------
dbl "$(named fonts.tar.gz)"
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
dbl 0
wait_for 'p[0]["dir"].endswith("/fonts.tar.gz/ttf")'
wait_for 'p[0]["rows"] == ["sub", "current.ttf", "Inter.ttf"]'
expect 'p[0]["rows"] == ["sub", "current.ttf", "Inter.ttf"]' "a symlink is a row like any other"

# --- copying out -------------------------------------------------------
key -k BackSpace
wait_for 'p[0]["dir"].endswith("/fonts.tar.gz")'
# The rows, before touching the selection. Waiting on `dir` alone returns the
# moment the pane starts moving, and a Down then lands on whatever row the old
# listing still had there. Which is how a copy ends up carrying something
# nobody chose.
wait_for 'p[0]["rows"] == ["ttf", "LICENSE"]'
key -k Down
wait_for 'p[0]["cursor"] == 0'
expect 'p[0]["rows"][0] == "ttf"' "the folder is the one selected"
key -M ctrl -P c -m ctrl

key -k BackSpace
# Named, and checked again on the line below, because the next keystroke
# writes to whatever directory this is.
wait_for "p[0][\"dir\"] == \"$START_DIR\""
expect "p[0][\"dir\"] == \"$START_DIR\"" "the paste target is the fixture directory"
key -M ctrl -P v -m ctrl

wait_for '"ttf" in p[0]["rows"]' 20
expect 'p[0]["row_count"] == 10' "the folder arrives beside the tarball"
expect 'not any(r.startswith(".hoja-") for r in p[0]["rows"])' "and nothing is left staged"
