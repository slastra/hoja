# A selection made while an archive is still being read.
#
#   SLOW=$(scripts/tests/setup-slow-archive.sh)
#   scripts/sway-harness.sh "$SLOW" scripts/tests/select-while-reading.sh
#
# Its own run rather than another block in `search-while-reading.sh`, because
# re-entering an archive in the same session answers from the cache with no
# thread at all — there is no "still reading" to act during the second time.
#
# The read tick rebuilds and re-sorts the rows underneath. Indices name
# positions, not files, so without carrying the selection across by identity a
# row picked here becomes a different member as later ones sort above it, and
# the next copy or delete acts on something nobody pointed at.

wait_for 'p[0]["rows"] == ["slow.tar.bz2"]'
dbl 0
wait_for 'p[0]["reading"] and p[0]["row_count"] >= 1' 30

key -k Down
wait_for 'p[0]["cursor"] == 0' 20
picked=$(probe 'p[0]["rows"][p[0]["cursor"]]')
echo "  ..   picked '$picked' while reading"

wait_for 'not p[0]["reading"]' 120
expect 'p[0]["row_count"] == 3' "the whole archive arrived after the pick"

now=$(probe 'p[0]["rows"][p[0]["cursor"]] if p[0]["cursor"] is not None else "none"')
if [ "$picked" = "$now" ]; then
    echo "  ok   the cursor is still on '$picked' once the rest arrived"
else
    echo "  FAIL the cursor is still on the row it was put on" >&2
    echo "       picked '$picked', ended on '$now'" >&2
    FAILED=$((FAILED + 1))
fi

sel=$(probe 'p[0]["rows"][p[0]["selected"][0]] if p[0]["selected"] else "none"')
if [ "$picked" = "$sel" ]; then
    echo "  ok   and so is the selection, which is what a copy would act on"
else
    echo "  FAIL the selection is still on the row it was put on" >&2
    echo "       picked '$picked', selection is '$sel'" >&2
    FAILED=$((FAILED + 1))
fi
