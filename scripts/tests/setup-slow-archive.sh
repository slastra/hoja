#!/usr/bin/env bash
# Build a tarball slow enough to read that a test can act while it is still
# going, in a directory of its own so it cannot change the row count the
# other archive fixtures assert on.
#
#   scripts/tests/setup-slow-archive.sh [dir]
#
# Real decompression time, not a debounce or a sleep: bzip2 is the slowest of
# the four codecs (measured elsewhere in this repo at roughly 39 MB/s), and
# the middle member here is ~15 MB of data incompressible enough that bzip2
# cannot shortcut the entropy coding, which takes on the order of a second to
# read back. That is long enough to span many `ARCHIVE_POLL` ticks (80 ms
# each) and short enough not to make the suite slow to run.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-slow-archive}"
rm -rf "$out"
mkdir -p "$out"

work="$out/.build"
mkdir -p "$work"
printf 'a' > "$work/aaa.txt"
printf 'z' > "$work/zzz.txt"
# Pseudo-random, not zeros: bzip2 flies through runs of the same byte, and the
# point is a member big enough that skipping past it takes real time.
head -c 15000000 /dev/urandom > "$work/mmm.bin"

tar -C "$work" --owner=0 --group=0 --numeric-owner --mtime='@1700000000' \
    --sort=name -cf "$out/slow.tar" aaa.txt mmm.bin zzz.txt
bzip2 -9 -f "$out/slow.tar"

rm -rf "$work"

echo "$out"
