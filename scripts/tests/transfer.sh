# Pausing and resuming a transfer.
#
# Run against the fixture `setup-transfer.sh` prints:
#
#   FIX=$(scripts/tests/setup-transfer.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/transfer.sh
#
# Keyboard only. The harness cannot synthesise a drag, so the transfer starts
# with ctrl-c and ctrl-v, and it is paused with its key rather than by clicking
# the toggle: a button's x would have to be derived from the window width, and
# being a few pixels out lands on the ✕ instead, which cancels the job and
# looks exactly like a pause that worked.
#
# Nothing here races the transfer. Four thousand files copy in about 170ms on
# this machine, so a test that pressed pause at a running job would be asking
# to lose. The fixture puts one colliding file in the destination instead: the
# worker blocks on the conflict prompt, which holds it still for as long as the
# test needs, and the pause is asked for before it is ever let go.

wait_for 'p[0]["rows"] == ["dst", "src"]'

# --- start it ------------------------------------------------------------
key -k Down
key -k Down
wait_for 'p[0]["cursor"] == 1'
expect 'p[0]["rows"][p[0]["cursor"]] == "src"' "the source tree is the one selected"
key -M ctrl -P c -m ctrl

dbl "$(named dst)"
wait_for 'p[0]["rows"] == ["src"]'
key -M ctrl -P v -m ctrl

# --- ask it to stop while it is standing still ---------------------------
wait_for 'd["modal"] == "conflict"' 30
expect 'd["jobs"][0]["state"] == "conflict"' "the transfer is waiting to be told what to do"

key -M ctrl -M shift -k space -m shift -m ctrl
wait_for 'd["jobs"][0]["state"] == "pausing"'
expect 'not d["jobs"][0]["done"]' "asked to stop, and not stopped yet"

# Enter is Replace. The worker is released, finishes the file it was on, and
# parks at the next one, which is what "between files" means.
key -k Return
wait_for 'd["jobs"][0]["state"] == "paused"' 30

# Two seconds is an order of magnitude longer than this copy needs, so a job
# that is still unfinished after it is one that genuinely stopped.
sleep 2
expect 'd["jobs"][0]["state"] == "paused"' "it is still parked two seconds later"
expect 'not d["jobs"][0]["done"]' "so the transfer really did stop, rather than racing past"

# --- let it go -----------------------------------------------------------
key -M ctrl -M shift -k space -m shift -m ctrl

# A finished job with nothing wrong is dropped from the strip, so an empty
# jobs list is what success looks like here.
wait_for 'len(d["jobs"]) == 0' 60
expect 'p[0]["rows"] == ["src"]' "the copy finished into the folder that was already there"

# Partials are hidden while they exist and gone afterwards either way, so a
# row starting with the prefix means one was orphaned.
expect 'not any(r.startswith(".hoja-") for r in p[0]["rows"])' "and nothing is left partial"

# --- undo ----------------------------------------------------------------
# The copy is on the undo stack, and ctrl-z takes it back. It runs as a job of
# its own, so a clean one leaving the strip is what finishing looks like.
#
# The `src` row stays: this paste *merged* into a folder that was already
# there, so the job never created it and undo has no business removing it.
# What undo owes is everything the job added, and the one file it replaced.
expect 'd["undo_depth"] == 1' "the transfer is on the undo stack"
key -M ctrl -P z -m ctrl
wait_for 'len(d["jobs"]) == 1' 20
wait_for 'len(d["jobs"]) == 0' 120
expect 'd["undo_depth"] == 0' "and taking it back empties the stack"

left=$(find "$START_DIR/dst" -type f | wc -l)
was=$(cat "$START_DIR/dst/src/d00/f0000.bin" 2>/dev/null)
if [ "$left" = "1" ] && [ "$was" = "older" ]; then
    echo "  ok   the destination is exactly as it was before the paste"
else
    echo "  FAIL the destination is exactly as it was before the paste" >&2
    echo "       $left file(s) left, the one that was replaced reads '$was'" >&2
    FAILED=$((FAILED + 1))
fi
