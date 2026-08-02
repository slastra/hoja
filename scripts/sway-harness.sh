#!/bin/bash
# Drive hoja inside a nested sway, for testing the interface without taking over
# the session you are sitting in.
#
#   scripts/sway-harness.sh <start-dir> <script-file> [out-dir]
#
# The script file is sourced with these in scope:
#
#   $OUT         where shots are written    shot NAME   the whole output
#   $W $H        the nested output size     strip NAME  just hoja's footer
#   row N        y of listing row N         click N     click listing row N
#   key ...      wtype arguments, e.g. -M ctrl -P a -m ctrl
#
# Nested rather than headless, so the sway window sits on your desktop and the
# test can be watched. Set HOJA_TEST_HEADLESS=1 to run it invisibly instead.
#
# The point of sway here is that all three tools scope to the nested compositor
# rather than to whatever the real session has focused:
#
#   grim    screenshots        WAYLAND_DISPLAY
#   wtype   virtual keyboard   WAYLAND_DISPLAY
#   wlrctl  virtual pointer    WAYLAND_DISPLAY
#
# `ydotool` cannot do this — it injects through the kernel's uinput, so its
# events land wherever the real session's focus happens to be. That is how an
# earlier version of these tests pasted a directory into a partition through
# somebody else's window.
#
# `swaymsg seat … cursor` looks like the answer and is not: with
# WLR_LIBINPUT_NO_DEVICES the seat has no pointer to move, and without it sway
# would take the real mouse. wlrctl creates a virtual one over the protocol,
# which needs no device at all.
set -u

START_DIR=${1:?usage: sway-harness.sh <start-dir> <script-file> [out-dir]}
BODY=${2:?usage: sway-harness.sh <start-dir> <script-file> [out-dir]}
OUT=${3:-$(mktemp -d)}
W=${HOJA_TEST_WIDTH:-1400}
H=${HOJA_TEST_HEIGHT:-900}
BIN=${HOJA_BIN:-./target/debug/hoja}

cd "$(dirname "$0")/.." || exit 1
[ -x "$BIN" ] || { echo "no binary at $BIN — cargo build first" >&2; exit 1; }
for tool in sway grim wtype wlrctl; do
    command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done
mkdir -p "$OUT"

HOST=${WAYLAND_DISPLAY:-}
# A socket nobody is listening on is one of ours from a run that was killed.
# Left in place it makes the new-socket detection below ambiguous.
for f in "$XDG_RUNTIME_DIR"/wayland-*; do
    case "$f" in *.lock) continue ;; esac
    [ "$(basename "$f")" = "$HOST" ] && continue
    fuser -s "$f" 2>/dev/null || rm -f "$f" "$f.lock"
done
BEFORE=$(ls "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null | tr '\n' ' ')

cat > "$OUT/sway.conf" <<'CONF'
# No decorations, so a listing row is where the arithmetic in `row` says it is.
default_border none
default_floating_border none
# Nothing to launch and nothing to grab: this session exists to hold one window.
CONF

if [ -n "${HOJA_TEST_HEADLESS:-}" ]; then
    BACKEND=headless
    # Without this the seat adopts the real input devices, which would take the
    # keyboard away from the session you are using.
    export WLR_LIBINPUT_NO_DEVICES=1
else
    BACKEND=wayland   # nested: sway appears as a window on the host
fi
# --unsupported-gpu: on an NVIDIA host sway otherwise paints a warning banner
# across the top, which shifts every row down by its height and makes `row`
# click the column header instead of the listing.
WLR_BACKENDS=$BACKEND WLR_WL_OUTPUTS=1 \
    sway --unsupported-gpu -c "$OUT/sway.conf" >"$OUT/sway.log" 2>&1 &
SWAY=$!
cleanup() { kill -9 "$SWAY" 2>/dev/null; }
trap cleanup EXIT

for _ in $(seq 1 60); do
    for f in "$XDG_RUNTIME_DIR"/wayland-*; do
        case "$f" in *.lock) continue ;; esac
        case " $BEFORE " in *" $f "*) continue ;; esac
        NESTED=$(basename "$f")
    done
    [ -n "${NESTED:-}" ] && break
    sleep 0.25
done
[ -z "${NESTED:-}" ] && { echo "sway never came up; see $OUT/sway.log" >&2; exit 1; }

export WAYLAND_DISPLAY=$NESTED
export SWAYSOCK
SWAYSOCK=$(ls -t "$XDG_RUNTIME_DIR"/sway-ipc.*.sock 2>/dev/null | head -1)
OUTPUT=$(swaymsg -t get_outputs 2>/dev/null |
    python3 -c "import sys,json;print(json.load(sys.stdin)[0]['name'])" 2>/dev/null)
swaymsg output "$OUTPUT" resolution "${W}x${H}" >/dev/null 2>&1
sleep 0.5
# Nested, the output takes the host window's size and ignores `resolution`, so
# the real figures come back from sway rather than from the request.
eval "$(swaymsg -t get_outputs 2>/dev/null | python3 -c "
import sys, json
r = json.load(sys.stdin)[0]['rect']
print(f\"W={r['width']}; H={r['height']}\")" 2>/dev/null)"

# Its own config and state, so a test can never write through to the real ones.
export XDG_STATE_HOME="$OUT/state" XDG_CONFIG_HOME="$OUT/config"
rm -rf "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
mkdir -p "$XDG_CONFIG_HOME/hoja"
printf '{ "theme": "Rosé Pine" }\n' > "$XDG_CONFIG_HOME/hoja/settings.json"

env -u DISPLAY "$BIN" "$START_DIR" >"$OUT/app.log" 2>&1 &
APP=$!
cleanup() { kill -9 "$APP" 2>/dev/null; kill -9 "$SWAY" 2>/dev/null; }
for _ in $(seq 1 40); do
    swaymsg -t get_tree 2>/dev/null | grep -q '"app_id": *"hoja"' && break
    sleep 0.25
done
sleep 1.5

# Where hoja actually sits, so the row arithmetic starts from its top edge
# rather than the output's — they differ by whatever chrome sway is drawing.
eval "$(swaymsg -t get_tree 2>/dev/null | python3 -c "
import sys, json
def find(n):
    if n.get('app_id') == 'hoja':
        return n['rect']
    for k in ('nodes', 'floating_nodes'):
        for c in n.get(k, []):
            if (r := find(c)):
                return r
r = find(json.load(sys.stdin)) or {'x': 0, 'y': 0, 'width': 0, 'height': 0}
print(f\"WX={r['x']}; WY={r['y']}; WW={r['width']}; WH={r['height']}\")" 2>/dev/null)"

# shellcheck disable=SC2317  # all sourced by the body, not called here
shot()  { grim "$OUT/$1.png" 2>/dev/null; }
# shellcheck disable=SC2317
strip() { grim -g "$WX,$((WY + WH - 26)) 700x26" "$OUT/$1.png" 2>&1 | head -1; }
# shellcheck disable=SC2317
# 28 toolbar + 24 header, then ROW_HEIGHT each, from hoja's own top edge.
row()   { echo $(( WY + 52 + 11 + $1 * 22 )); }
# shellcheck disable=SC2317
click() {
    wlrctl pointer move 100000 100000 >/dev/null 2>&1   # corner, then absolute-ish
    wlrctl pointer move -100000 -100000 >/dev/null 2>&1
    wlrctl pointer move "$((WX + 150))" "$(row "$1")" >/dev/null 2>&1
    wlrctl pointer click left >/dev/null 2>&1
}
# shellcheck disable=SC2317
key()   { wtype "$@"; }

# shellcheck source=/dev/null
. "$BODY"

echo "$OUT"
