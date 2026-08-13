# Phase two: the same state directory, a new process. The record the dead run
# left is picked up, its half-written files are already gone, and the transfer
# is offered back.
wait_for 'd["interrupted"] == 1' 20
expect 'd["interrupted"] == 1' "the interrupted transfer is offered back"
expect 'not any(r.startswith(".hoja-") for r in p[0]["rows"])' "and its partial files were reaped"

# Accept it. The offer is the only row in the strip, so it sits on the bottom
# edge, and Finish is the button left of the ✕ — measured from a screenshot
# found by sweeping rather than derived, because the two are laid out by their
# text and a screenshot measured it wrong. If the
# layout moves, this misses and the wait below fails loudly; it cannot pass by
# hitting the wrong thing, since dismissing would leave no job behind.
at $((WW - 40)) $((WH - 13))
wait_for 'len(d["jobs"]) == 1' 20
expect 'd["interrupted"] == 0' "accepting the offer takes it off the strip"

# It asks, rather than deciding. The interrupted run was never told what to do
# about the collision — it died waiting — so neither is this one. Answering
# Skip on its behalf would have reported a clean finish while the file the user
# was being asked about still held its old contents.
wait_for 'd["modal"] == "conflict"' 30
expect 'd["jobs"][0]["state"] == "conflict"' "and asks the question the dead run died on"
key -k Return

wait_for 'len(d["jobs"]) == 0' 90

# Asserted against the filesystem rather than the listing: this pane is at the
# fixture root, and what was finished is two directories below it.
copied=$(find "$START_DIR/dst" -type f | wc -l)
if [ "$copied" = "4000" ]; then
    echo "  ok   and the transfer finished what the dead run had left"
else
    echo "  FAIL and the transfer finished what the dead run had left" >&2
    echo "       $copied of 4000 files at the destination" >&2
    FAILED=$((FAILED + 1))
fi
