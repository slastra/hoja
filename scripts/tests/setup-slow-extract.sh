#!/usr/bin/env bash
# Build the fixture extract-cancel.sh runs against.
#
#   scripts/tests/setup-slow-extract.sh [dir]
#
# A bzip2 tarball whose last member sits behind 80 MB of random bytes, so
# taking it out means decompressing all of them first: seconds of work, long
# enough to cancel in the middle of. Its own fixture rather than a bigger
# `setup-slow-archive.sh`, which other suites read whole.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-slow-extract}"
rm -rf "$out"
mkdir -p "$out"

work="$out/.build"
mkdir -p "$work"
head -c 80000000 /dev/urandom > "$work/mmm.bin"
printf 'z' > "$work/zzz.txt"

tar -C "$work" --owner=0 --group=0 --numeric-owner --mtime='@1700000000' \
    --sort=name -cf "$out/slow.tar" mmm.bin zzz.txt
bzip2 -1 -f "$out/slow.tar"

rm -rf "$work"

echo "$out"
