#!/usr/bin/env bash
# Build the fixture the columns test runs against.
#
#   scripts/tests/setup-columns.sh [dir]
#
# Two rows whose modes are set rather than inherited, because the test asserts
# the exact string the Permissions column prints. `chmod` says what `umask`
# would only guess at, and a fixture built under a different umask would print
# something else and fail for no reason.
#
# Owner and group are deliberately not asserted anywhere: they are whoever ran
# the script, named out of this machine's `/etc/passwd`, and a test that pins
# them passes on one developer's box.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-columns}"
rm -rf "$out"
mkdir -p "$out/a-folder"
: > "$out/b-script"
chmod 0755 "$out/b-script"
chmod 0750 "$out/a-folder"
# A third row with something in it, so that sorting by size is observable at
# all: empty files sort into the same order as their names and would let a
# header click that did nothing pass.
head -c 4096 /dev/zero > "$out/c-big"
echo "$out"
