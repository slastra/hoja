# The second half of grid.sh: a restart against the state it left.
#
#   OUT=$(mktemp -d)
#   scripts/sway-harness.sh "$FIX" scripts/tests/grid.sh "$OUT"
#   HOJA_TEST_KEEP_STATE=1 scripts/sway-harness.sh "$FIX" scripts/tests/grid-restore.sh "$OUT"

wait_for 'p[0]["row_count"] == 44'
expect 'p[0]["layout"] == "grid"' "a restart keeps the grid"
expect 'p[0]["card_details"] == ["size", "modified"]' "and the card details"
