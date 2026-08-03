# A resort clicked while a slow tarball is still being read must not be
# silently overwritten by the next batch of rows landing.
#
#   FIX=$(scripts/tests/setup-slow-archive.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/resort-while-reading.sh
#
# `spawn_archive_read`'s poll loop used to capture `self.view.sort` once, before
# its loop started, and reuse that stale copy on every tick including the last
# one. A resort issued mid-read took effect for exactly one frame and then was
# gone, clobbered by the read's own next rebuild.

wait_for 'p[0]["row_count"] == 1'

dbl 0
wait_for 'p[0]["dir"].endswith("/slow.tar.bz2")'

# The window this bug lives in: past the tiny first file and the 15 MB one
# right after it, but bzip2 is still working through the file that follows,
# which is what makes this reliably still true a moment from now rather than
# a race against the whole read finishing first.
wait_for 'p[0]["reading"] and p[0]["row_count"] >= 2' 10
expect 'p[0]["rows"][:2] == ["aaa.txt", "mmm.bin"]' "ascending by name, the default, while still reading"

# No direct keybinding for this one: it is a header click or a palette
# command, and the palette is the one a script can drive without guessing at
# a header's pixel position.
key -M ctrl -M shift -P p -m shift -m ctrl
key reverse sort
key -k Return

wait_for 'not p[0]["reading"]' 15
expect 'p[0]["rows"] == ["zzz.txt", "mmm.bin", "aaa.txt"]' "the resort survived to the end of the read"
