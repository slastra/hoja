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
#   named NAME   index of the row called NAME            right N  right-click row N
#   key ...      wtype arguments, e.g. -M ctrl -P a -m ctrl
#
# Nested rather than headless, so the sway window sits on your desktop and the
# test can be watched. Set HOJA_TEST_HEADLESS=1 to run it invisibly instead.
#
# Nested, the window presents to the host as class `wlroots`, and a tiling host
# will stretch it to whatever slot it lands in — which moves every row the test
# clicks. Float it at a fixed size. Under Hyprland:
#
#   hl.window_rule({ name = "nested-wlroots", match = { class = "^(wlroots)$" },
#                    float = true, size = "1400 900", center = true })
#
# Without a rule the harness still runs; it reads the real geometry back rather
# than assuming, so only the stability of the coordinates suffers.
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

# What the window is showing, rewritten whenever it changes. This is what lets
# a test assert and wait rather than screenshot and sleep; see `src/probe.rs`.
export HOJA_PROBE="$OUT/probe.json"
rm -f "$HOJA_PROBE"

# Its own config, state and data, so a test can never write through to the real
# ones. Data matters as much as the other two: the command palette's history
# lives there, and the trash a delete moves files into is
# `$XDG_DATA_HOME/Trash`, so without this a run would put the developer's own
# files and a test's alike into one bin.
export XDG_STATE_HOME="$OUT/state" XDG_CONFIG_HOME="$OUT/config" XDG_DATA_HOME="$OUT/data"
rm -rf "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"
mkdir -p "$XDG_CONFIG_HOME/hoja" "$XDG_DATA_HOME"
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
# right N: right-click listing row N, or empty space below the last row for
# an index past the end, to open (or fail to open) a context menu.
right() {
    wlrctl pointer move 100000 100000 >/dev/null 2>&1
    wlrctl pointer move -100000 -100000 >/dev/null 2>&1
    wlrctl pointer move "$((WX + 150))" "$(row "$1")" >/dev/null 2>&1
    wlrctl pointer click right >/dev/null 2>&1
}
# shellcheck disable=SC2317
key()   { wtype "$@"; }

# --- pointer -----------------------------------------------------------------
# wlrctl's absolute positioning is a corner reset followed by a relative move,
# which is why every one of these starts by slamming into the top-left.

# shellcheck disable=SC2317
# at X Y: click a point in window coordinates.
at() {
    wlrctl pointer move 100000 100000 >/dev/null 2>&1
    wlrctl pointer move -100000 -100000 >/dev/null 2>&1
    wlrctl pointer move "$((WX + $1))" "$((WY + $2))" >/dev/null 2>&1
    wlrctl pointer click left >/dev/null 2>&1
}
# shellcheck disable=SC2317
# dbl N: double-click listing row N. One move, two clicks: a reset between them
# reads as two singles.
dbl() {
    wlrctl pointer move 100000 100000 >/dev/null 2>&1
    wlrctl pointer move -100000 -100000 >/dev/null 2>&1
    wlrctl pointer move "$((WX + 150))" "$(row "$1")" >/dev/null 2>&1
    wlrctl pointer click left >/dev/null 2>&1
    wlrctl pointer click left >/dev/null 2>&1
}
# shellcheck disable=SC2317
# divider N: x of the handle left of fixed column N, counting 0 from the right.
# Derived from the app's own constants rather than hunted for: the handle is
# 5px wide, and being three out lands on the header instead, which silently
# toggles the sort and looks like the drag did nothing.
divider() {
    python3 -c "
import sys
widths = [100, 90, 132]           # Column::default_width, left to right
handle = 5
right = int(sys.argv[1]) + int(sys.argv[2])
edge = right
for w in reversed(widths[int(sys.argv[3]):]):
    edge -= w + handle
print(edge + handle // 2)" "$WX" "$WW" "$1"
}

# --- the probe ---------------------------------------------------------------

# shellcheck disable=SC2317
# probe EXPR: evaluate a python expression over the probe. `d` is the document,
# `p` the panes. Prints the result, or nothing if the file is not there yet.
probe() {
    python3 -c "
import json, sys, pathlib
f = pathlib.Path(sys.argv[1])
if not f.exists():
    sys.exit(1)
d = json.loads(f.read_text())
p = d['panes']
print(eval(sys.argv[2]))" "$HOJA_PROBE" "$1" 2>/dev/null
}

# shellcheck disable=SC2317
# named NAME: the index of the listing row called NAME, for `dbl` and `click`.
#
# Counting rows by hand is how a test ends up double-clicking the file next to
# the one it meant, which is silent when both are archives. Deliberately not
# called `row`: that one is the harness's own y-coordinate helper, and shadowing
# it makes every click land at a coordinate derived from a python expression.
named() { probe "p[0][\"rows\"].index(\"$1\")"; }

# shellcheck disable=SC2317
# wait_for EXPR [seconds]: poll until the expression is true. Replaces a sleep
# that was guessing; a test that waits for what it needs does not get slower on
# a loaded machine and does not pass by accident on a fast one.
#
# A timeout ENDS THE TEST, and that is not a matter of tidiness. A `wait_for`
# is what a script knows about where the window is; once one has failed, the
# script no longer knows, and everything after it is keystrokes sent somewhere
# unverified. A run that lost its place while walking up out of a fixture
# directory carried on to the root of the filesystem and pressed paste there,
# and the only thing that stopped it was that root is not writable. That is
# luck, and luck is not a test harness.
#
# `expect` still only counts a failure: an assertion that is wrong about what is
# on screen has not lost track of where the window is.
wait_for() {
    local deadline=$(( SECONDS + ${2:-10} ))
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ "$(probe "$1")" = "True" ] && return 0
        sleep 0.1
    done
    echo "TIMEOUT waiting for: $1" >&2
    echo "       stopping here: what is on screen is no longer known, and the" >&2
    echo "       rest of this script would be typing into somewhere unchecked." >&2
    FAILED=$((FAILED + 1))
    echo "$FAILED check(s) failed; artifacts in $OUT" >&2
    echo "$OUT"
    exit 1
}

# shellcheck disable=SC2317
# expect EXPR DESCRIPTION: assert, and remember if it failed.
expect() {
    if [ "$(probe "$1")" = "True" ]; then
        echo "  ok   $2"
    else
        echo "  FAIL $2" >&2
        echo "       $1 -> $(probe "$1")" >&2
        FAILED=$((FAILED + 1))
    fi
}

FAILED=0

# shellcheck source=/dev/null
. "$BODY"

if [ "$FAILED" -gt 0 ]; then
    echo "$FAILED check(s) failed; artifacts in $OUT" >&2
    echo "$OUT"
    exit 1
fi
echo "$OUT"
