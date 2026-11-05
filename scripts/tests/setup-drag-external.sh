#!/usr/bin/env bash
# Build the fixture the drag-external test runs against.
#
#   scripts/tests/setup-drag-external.sh [dir]
#
# Two folders, one shown by hoja and one by nemo, each holding the file that
# is dragged to the other, and a zip on the hoja side. Prints the hoja side; the nemo side is beside it.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-drag-external}"
rm -rf "$out"
mkdir -p "$out/hoja-side" "$out/nemo-side"
echo out > "$out/hoja-side/outbound.txt"
echo in > "$out/nemo-side/inbound.txt"
# An archive, for the drag that is extracted only when the target asks: what
# lands in nemo has to be the member, never the zip around it.
python3 - "$out/hoja-side/pack.zip" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w") as z:
    z.writestr("member.txt", "from inside\n")
PY
echo "$out/hoja-side"
