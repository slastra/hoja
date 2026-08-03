# Navigating away from an archive mid-read must not leave the pane thinking
# it is still reading one, forever.
#
#   FIX=$(scripts/tests/setup-slow-archive.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/interrupt-archive-read.sh
#
# `reading_bytes` used to be cleared only inside `spawn_archive_read`'s own
# "finished" branch. Navigating away drops that task before it ever reaches
# that branch, abandoned mid-loop rather than completed, so nothing else ever
# set it back to `None`: the footer of every directory visited afterward kept
# reading "reading… N items" for a load that had nothing to do with any
# archive, for the rest of that pane's life.

wait_for 'p[0]["row_count"] == 1'

dbl 0
wait_for 'p[0]["dir"].endswith("/slow.tar.bz2")'

# The same mid-read window `resort-while-reading.sh` uses: past the first two
# members, bzip2 still working through the one after them.
wait_for 'p[0]["reading"] and p[0]["row_count"] >= 2' 10

key -k BackSpace
wait_for "p[0][\"dir\"] == \"$START_DIR\""

expect 'not p[0]["reading"]' "a real directory does not read as still reading an archive"
expect '"reading" not in p[0]["footer"]' "and the footer agrees"
