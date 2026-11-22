# Copying out of an archive shows in the strip, and stops when cancelled.
#
#   FIX=$(scripts/tests/setup-slow-extract.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/extract-cancel.sh
#
# The member asked for is the last one, behind 80 MB that bzip2 has to work
# through first, so the extraction is still passing over the first member
# when it is cancelled. Cancelling used to wait for that to end, and before
# that there was nothing to cancel with: the strip said "Extracting…" and
# nothing else.

wait_for 'p[0]["row_count"] == 1'
dbl 0
wait_for 'p[0]["dir"].endswith("/slow.tar.bz2")'
wait_for 'p[0]["rows"] == ["mmm.bin", "zzz.txt"] and not p[0]["reading"]' 60

key -k End
wait_for 'p[0]["cursor"] == 1'
key -M ctrl -P c -m ctrl
key -k BackSpace
wait_for "p[0][\"dir\"] == \"$START_DIR\" and p[0][\"row_count\"] == 1"
key -M ctrl -P v -m ctrl

wait_for 'd["extractions"] == 1' 10
expect 'd["extractions"] == 1' "the extraction has a row in the strip"
shot extracting

# The close button is the last thing on the row, at the right edge.
at $((WW - 15)) $((WH - 13))
wait_for 'd["extractions"] == 0' 3
expect 'd["extractions"] == 0' "cancelling stops it at once"
sleep 1
expect 'p[0]["rows"] == ["slow.tar.bz2"]' "and nothing came out of the archive"
staged=$(find "$START_DIR" -maxdepth 1 -name '.hoja-*' | wc -l)
if [ "$staged" = "0" ]; then
    echo "  ok   and nothing is left staged"
else
    echo "  FAIL and nothing is left staged" >&2
    FAILED=$((FAILED + 1))
fi
expect 'd["notice"] is None' "and a cancel asked for says nothing"
