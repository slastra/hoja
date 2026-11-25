#!/usr/bin/env bash
# Retake the README's pictures: docs/header.png, docs/preview.png,
# docs/palette.png and docs/picker.png.
#
#   cargo build && scripts/readme-shots/run.sh [DEMO]
#
# DEMO (default ~/Mock) is made fresh for the shots and removed after. Its path
# is what the toolbars show, which is why it is not a temporary directory.
#
# Needs bgen, for the pictures, and Hyprland. Nested, the sway output takes
# the size of its window on the host and ignores `resolution`, so each shot
# resizes that window with hyprctl (see size.sh). Keep the sizes multiples of
# 16: grim fails with "Invalid stride" at some odd widths.
set -euo pipefail
cd "$(dirname "$0")/../.."
HERE=scripts/readme-shots
export DEMO=${1:-$HOME/Mock}
[ -e "$DEMO" ] && { echo "$DEMO exists; remove it or name another" >&2; exit 1; }
OUT=$(mktemp -d)
trap 'rm -rf "$DEMO" "$OUT"' EXIT

"$HERE/demo.sh" "$DEMO"
scripts/sway-harness.sh "$DEMO/Projects/garden" "$HERE/header.sh" "$OUT/header"
scripts/sway-harness.sh "$DEMO/Projects/garden/src" "$HERE/preview.sh" "$OUT/preview"
scripts/sway-harness.sh "$DEMO/Pictures" "$HERE/palette.sh" "$OUT/palette"
scripts/sway-harness.sh "$DEMO/Pictures" "$HERE/picker.sh" "$OUT/picker"

python3 -I "$HERE/frame.py" "$OUT/header/raw.png" docs/header.png --logo docs/hoja-dark.png 112 300 720
python3 -I "$HERE/frame.py" "$OUT/preview/raw.png" docs/preview.png
python3 -I "$HERE/frame.py" "$OUT/palette/raw.png" docs/palette.png
python3 -I "$HERE/frame.py" "$OUT/picker/raw.png" docs/picker.png
echo "docs/header.png docs/preview.png docs/palette.png docs/picker.png"
