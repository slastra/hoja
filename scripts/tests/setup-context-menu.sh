#!/usr/bin/env bash
# Build the fixture the context-menu test runs against.
#
#   scripts/tests/setup-context-menu.sh [dir]
#
# One PNG, and one directory beside it.
#
# The file has to be a *real* file of a type the system has handlers for:
# "Open With" is built from the XDG mime database, so a row with no registered
# applications produces no submenu and the test would pass by proving nothing.
#
# A PNG rather than a text file, and that matters. The submenu exists because
# the flat list was capped at eight and silently dropped the rest; proving the
# cap is gone needs a type that beats it. text/plain draws about six handlers
# here, which cannot tell the two apart. Images draw editors, viewers, browsers
# and converters — twelve on this machine.
#
# The directory is the control. `open_context_menu` only offers Open/Open With
# for a row that has a file behind it, so this is the row that must *not* grow
# a submenu.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-context-menu}"
rm -rf "$out"
mkdir -p "$out/a-folder"
# A 1x1 PNG, written by hand so the fixture needs no image tooling.
python3 - "$out/shot.png" <<'PNG'
import struct, sys, zlib
raw = b"\x00\xff\x00\x00"
def chunk(tag, data):
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw, 9))
       + chunk(b"IEND", b""))
open(sys.argv[1], "wb").write(png)
PNG
echo "$out"
