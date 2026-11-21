#!/usr/bin/env bash
# Build the fixture the big-grid test runs against.
#
#   scripts/tests/setup-grid-big.sh [dir]
#
# 1500 images in seven shapes, which is more than the preview cache keeps, so
# a test can page past the point where it starts letting previews go. Plain
# Python writes them, one solid colour each, with no imaging library.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-grid-big}"
rm -rf "$out"
mkdir -p "$out"
python3 -I - "$out" <<'PY'
import struct, sys, zlib
out = sys.argv[1]
shapes = [(64, 40), (30, 60), (50, 50), (80, 30), (40, 56), (60, 45), (36, 64)]
def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
for i in range(1500):
    w, h = shapes[i % len(shapes)]
    pixel = bytes(((i * 37) % 255, (i * 91) % 255, (i * 53) % 255))
    rows = b"".join(b"\0" + pixel * w for _ in range(h))
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(rows, 1))
           + chunk(b"IEND", b""))
    with open(f"{out}/img-{i:04d}.png", "wb") as f:
        f.write(png)
PY
echo "$out"
