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
