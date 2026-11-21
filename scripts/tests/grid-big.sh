# The grid in a folder larger than the preview cache: cards must hold still.
#
#   FIX=$(scripts/tests/setup-grid-big.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/grid-big.sh
#
# A card moves when a card above it in its column changes shape. Paging down,
# after each page the probe is read every 50ms for 2.5s, and a card that
# moved relative to the others while the view was still counts. Past the
# 400th card the cache starts letting previews go; a card whose shape went
# with its preview fell back to a guess, and the cards under it moved. That
# must not happen, and neither must jitter after a jump to the end.

WATCH=scripts/tests/watch-shifts.py
wait_for 'p[0]["row_count"] == 1500' 30
key -M ctrl -k 2 -m ctrl
wait_for 'p[0]["grid_columns"] >= 2'
card 0

# The first page may still be settling: the shapes are read in the
# background from the top, and the grid opened a moment ago.
key -k Next
python3 "$WATCH" "$HOJA_PROBE" 2.5 >/dev/null
moved=0
for _ in $(seq 1 25); do
    key -k Next
    moved=$((moved + $(python3 "$WATCH" "$HOJA_PROBE" 2.5)))
done
expect "p[0]['cursor'] > 450 and $moved == 0" "paging past the cache's size, no card moves ($moved)"

key -k End
python3 "$WATCH" "$HOJA_PROBE" 3 >/dev/null
settled=$(python3 "$WATCH" "$HOJA_PROBE" 3)
expect "$settled == 0" "after a jump to the end, the cards settle and hold still ($settled)"

key -k Home
back=$(python3 "$WATCH" "$HOJA_PROBE" 3)
expect "p[0]['top_row'] == 0 and $back == 0" "and going back to the top moves nothing ($back)"
