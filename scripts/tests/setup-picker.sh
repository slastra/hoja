#!/usr/bin/env bash
# Build the fixture the picker tests run against.
#
#   scripts/tests/setup-picker.sh [dir]
#
# Two pictures, a text file, a model, a zip and a folder. Written by Python
# with the standard library alone. Prints the folder.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-picker}"
rm -rf "$out"
mkdir -p "$out/sub"

python3 -I - "$out" <<'PY'
import os, struct, sys, zipfile, zlib

out = sys.argv[1]

def png(w, h, rgb):
    """A plain PNG of one color, written by hand."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
    raw = b"".join(b"\0" + bytes(rgb) * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw))
            + chunk(b"IEND", b""))

def write(name, data):
    with open(os.path.join(out, name), "wb") as f:
        f.write(data)

write("photo.png", png(8, 8, (200, 80, 60)))
write("sub/deep.png", png(8, 8, (60, 80, 200)))
write("notes.txt", b"a note\n")
write("part.stl", b"solid t\nendsolid t\n")
with zipfile.ZipFile(os.path.join(out, "bundle.zip"), "w") as z:
    z.writestr("inside.png", png(4, 4, (0, 0, 0)))
PY

echo "$out"
