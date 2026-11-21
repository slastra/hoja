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
printf 'fn main() {\n\tprintln!("hello");\n}\n' > "$out/notes.rs"
# Not text, not media: no tooltip.
head -c 4096 /dev/urandom > "$out/blob.bin"
echo "$out"
