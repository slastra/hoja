# Searching an archive that is still being read.
#
#   SLOW=$(scripts/tests/setup-slow-archive.sh)
#   scripts/sway-harness.sh "$SLOW" scripts/tests/search-while-reading.sh
#
# `slow.tar.bz2` holds a member large enough to take many read ticks, which is
# the only way to be inside this window long enough to type.

wait_for 'p[0]["rows"] == ["slow.tar.bz2"]'
dbl 0
wait_for 'p[0]["reading"]' 20

# Search while members are still arriving. The results are what has been read
# so far, and the footer says so rather than stating a total that is about to
# change.
key -M ctrl -P f -m ctrl
sleep 0.3
key t
wait_for 'p[0]["searching"]' 20
expect 'p[0]["footer"].startswith("searching…")' "a count while reading is a count so far"

# The read goes on underneath. It used to overwrite the results with the whole
# listing every eighty milliseconds, leaving the footer counting matches for
# rows nobody had searched; now they grow with it and settle.
wait_for 'not p[0]["reading"]' 120
wait_for 'p[0]["footer"] == "2 matches"' 20
expect 'p[0]["rows"] == ["aaa.txt", "zzz.txt"]' "the results are results, not the listing"
expect 'p[0]["footer"] == "2 matches"' "and the count settles once the read is done"

key -k Escape
wait_for 'not p[0]["searching"]'
expect 'p[0]["row_count"] == 3' "escape puts the whole archive back"
