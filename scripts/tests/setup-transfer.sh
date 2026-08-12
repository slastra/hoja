#!/usr/bin/env bash
# Build the fixture directory the transfer test runs against.
#
#   scripts/tests/setup-transfer.sh [dir]
#
# A source tree, and a destination holding exactly one file that collides with
# it.
#
# The collision is the point. Measured on this machine, hoja copies these four
# thousand files in about 170ms, and no fixture that fits in a repository is
# going to be slow enough to catch mid-flight: tmpfs and the page cache between
# them mean the bytes are never the bottleneck, and reaching several seconds
# would take something like a hundred and twenty thousand files. So the test
# does not race the transfer at all. It stops it on a conflict, which blocks
# the worker until someone answers, and asks for the pause while it is standing
# still.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-transfer}"
rm -rf "$out"
mkdir -p "$out/src" "$out/dst"

python3 - "$out" <<'PY'
import sys
from pathlib import Path

out = Path(sys.argv[1])
src = out / "src"
# Real content rather than zeros: a hole is not a copy, and the sparse path
# would otherwise be doing the work instead of the tier ladder.
body = bytes(range(256)) * 16
for i in range(4000):
    d = src / f"d{i // 200:02d}"
    d.mkdir(exist_ok=True)
    (d / f"f{i:04d}.bin").write_bytes(body)

# The one the transfer stops on. Directories merge without asking, so the
# collision has to be a file; `process_dir` sorts its entries by name, so this
# is the first thing the job reaches and it blocks almost immediately.
# Different contents, so overwriting it is observable.
first = out / "dst" / "src" / "d00" / "f0000.bin"
first.parent.mkdir(parents=True)
first.write_bytes(b"older")
PY

echo "$out"
