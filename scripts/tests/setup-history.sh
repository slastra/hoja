#!/usr/bin/env bash
# Build the fixture the history test runs against.
#
#   scripts/tests/setup-history.sh [dir]
#
# Eighty folders, more than a window shows, so that the end of the listing is
# scrolled: Back has a scroll position to put back, not only a cursor.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-history}"
rm -rf "$out"
mkdir -p "$out"
for i in $(seq -w 0 79); do
    mkdir "$out/f$i"
    : > "$out/f$i/inside.txt"
done
echo "$out"
