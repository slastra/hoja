#!/usr/bin/env bash
# Build the fixture the preview test runs against.
#
#   scripts/tests/setup-preview.sh [dir]
#
# One file of each kind that gets a preview, and one that does not. The media
# are made here with ffmpeg rather than checked in. Prints the folder.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-preview}"
rm -rf "$out"
mkdir -p "$out"
ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }
# Larger than a thumbnail, so the picture has to be shrunk.
ff -f lavfi -i testsrc=size=640x400:rate=1 -frames:v 1 "$out/photo.png"
ff -f lavfi -i testsrc=size=320x240:rate=25 -t 2 -pix_fmt yuv420p "$out/clip.mp4"
# The same frames, turned a quarter as a phone held upright turns them. The
# stored frames stay 320x240 and the header says to show them 240x320.
ff -display_rotation 90 -i "$out/clip.mp4" -c copy "$out/portrait.mp4"
ff -f lavfi -i sine=frequency=440:duration=3 -c:a libvorbis \
    -metadata title="Test Tone" -metadata artist="Hoja" -metadata album="Fixtures" \
    "$out/tone.ogg"
# Two landscape Letter pages, so the page count and a shape other than the
# portrait default are both checked.
python3 -I - "$out/report.pdf" <<'PY'
import sys
# Two landscape pages, each a colored band and a box, written by hand so the
# fixture needs no PDF library. Offsets in the xref are computed, not typed.
pages = [b"0.2 0.4 0.8 rg 40 300 712 160 re f 0 0 0 RG 4 w 40 40 712 200 re S",
         b"0.8 0.3 0.2 rg 40 40 712 512 re f"]
objs = [b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 792 612] >>"]
for i, content in enumerate(pages):
    page_no = 3 + 2 * i
    objs.append(b"<< /Type /Page /Parent 2 0 R /Contents %d 0 R >>" % (page_no + 1))
    objs.append(b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream")
out = bytearray(b"%PDF-1.4\n")
offsets = []
for n, body in enumerate(objs, 1):
    offsets.append(len(out))
    out += b"%d 0 obj\n" % n + body + b"\nendobj\n"
xref = len(out)
out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
for off in offsets:
    out += b"%010d 00000 n \n" % off
out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
open(sys.argv[1], "wb").write(out)
PY
printf 'fn main() {\n\tprintln!("hello");\n}\n' > "$out/notes.rs"
# Not text, not media: no tooltip.
head -c 4096 /dev/urandom > "$out/blob.bin"
echo "$out"
