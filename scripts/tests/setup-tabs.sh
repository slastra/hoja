#!/usr/bin/env bash
# Build the fixture the tabs test runs against.
#
#   scripts/tests/setup-tabs.sh [dir]
#
# Two folders, so a tab has somewhere to go that the other one is not.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-tabs}"
rm -rf "$out"
mkdir -p "$out/alpha" "$out/beta"
: > "$out/alpha/one.txt"
: > "$out/beta/two.txt"
echo "$out"
