# Crash recovery, in two runs against one state directory.
#
#   FIX=$(scripts/tests/setup-transfer.sh)
#   OUT=$(mktemp -d)
#   scripts/sway-harness.sh "$FIX" scripts/tests/crash-phase1.sh "$OUT"
#   HOJA_TEST_KEEP_STATE=1 \
#       scripts/sway-harness.sh "$FIX" scripts/tests/crash-phase2.sh "$OUT"
#
# The harness kills the app when a script returns, so phase one ending *is*
# the crash. HOJA_TEST_KEEP_STATE stops phase two wiping what phase one left,
# which is the only reason this can be tested at all.
#
wait_for 'p[0]["rows"] == ["dst", "src"]'
key -k Down
key -k Down
wait_for 'p[0]["cursor"] == 1'
key -M ctrl -P c -m ctrl
dbl "$(named dst)"
wait_for 'p[0]["rows"] == ["src"]'
key -M ctrl -P v -m ctrl
# Stopped on the pre-placed collision, so it is provably still running — and
# so its record is still on disk — when the process goes.
wait_for 'd["modal"] == "conflict"' 30
expect 'len(d["jobs"]) == 1' "a transfer is in flight when the process dies"
