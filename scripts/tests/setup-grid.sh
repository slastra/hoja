#!/usr/bin/env bash
# Build the fixture the grid test runs against.
#
#   scripts/tests/setup-grid.sh [dir]
#
# More than a screen of cards in many shapes: images wide, tall and square,
# a video in each orientation, a PDF, audio, text, and folders. Made with
# ffmpeg, and the PDF from the preview fixture. Prints the folder.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-grid}"
rm -rf "$out"
mkdir -p "$out/albums" "$out/notes"
ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }
# Shapes in turn, so neighbors differ and the columns end ragged.
shapes=(640x400 300x600 500x500 800x300 400x560)
for i in $(seq -w 1 36); do
    size=${shapes[$((10#$i % ${#shapes[@]}))]}
    ff -f lavfi -i "testsrc=size=$size:rate=1" -frames:v 1 "$out/img-$i.png"
done
ff -f lavfi -i testsrc=size=320x240:rate=25 -t 2 -pix_fmt yuv420p "$out/clip.mp4"
ff -display_rotation 90 -i "$out/clip.mp4" -c copy "$out/portrait.mp4"
ff -f lavfi -i sine=frequency=440:duration=3 -c:a libvorbis -metadata title="Tone" "$out/tone.ogg"
printf 'one\ntwo\n' > "$out/readme.txt"
printf 'fn main() {}\n' > "$out/main.rs"
# The PDF comes from the preview fixture's script, which writes one by hand.
preview=$(mktemp -d)
"$(dirname "$0")/setup-preview.sh" "$preview" >/dev/null
cp "$preview/report.pdf" "$out/report.pdf"
rm -rf "$preview"
echo "$out"
