#!/bin/bash
# Drive hoja on a private X display, for testing the interface without touching
# the one you are sitting in front of.
#
#   scripts/x11-harness.sh <start-dir> <script-file> [out-dir]
#
# The script file is sourced with these in scope:
#
#   $WIN            the window id           $OUT       where shots are written
#   $WIDTH $HEIGHT  the window size         shot NAME  the whole window
#   row N           y of listing row N      strip NAME just the footer
#   click N         click listing row N     key ...    xdotool key, e.g. ctrl+a
#
# Why an X display and not a nested Wayland compositor: input. `ydotool` injects
# through the kernel's uinput, so its events land on whichever window the real
# session has focused — which means a test steals your focus and can type into
# your windows. `xdotool` speaks the X protocol and is scoped by `$DISPLAY`, as
# is `import`, so both the keystrokes and the screenshots stay inside a display
# that is not on any screen. hoja already builds with gpui's x11 backend, so
# nothing has to change for it to run here.
#
# The alternative that also works is `sway` headless plus `swaymsg seat - cursor`,
# which is the only Wayland compositor exposing scoped pointer injection. It
# needs installing; this does not.
set -u

START_DIR=${1:?usage: x11-harness.sh <start-dir> <script-file> [out-dir]}
BODY=${2:?usage: x11-harness.sh <start-dir> <script-file> [out-dir]}
OUT=${3:-$(mktemp -d)}
DISP=${HOJA_TEST_DISPLAY:-:99}
BIN=${HOJA_BIN:-./target/debug/hoja}

cd "$(dirname "$0")/.." || exit 1
[ -x "$BIN" ] || { echo "no binary at $BIN — cargo build first" >&2; exit 1; }
mkdir -p "$OUT"

# The bracket keeps the pattern from matching this script's own command line,
# which would otherwise kill the shell running it.
pkill -f "[X]vfb $DISP" 2>/dev/null
sleep 0.3
# -dpi 96 holds gpui at scale 1, so a listing row is ROW_HEIGHT pixels tall and
# `row` below can do arithmetic rather than guesswork.
Xvfb "$DISP" -screen 0 1920x1200x24 -dpi 96 >/dev/null 2>&1 &
XVFB=$!
sleep 1.5

# Its own config and state, so a test can never write through to the real ones.
# An earlier version did, and left a 500x702 window size as the remembered
# default.
export XDG_STATE_HOME="$OUT/state" XDG_CONFIG_HOME="$OUT/config"
rm -rf "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
mkdir -p "$XDG_CONFIG_HOME/hoja"
printf '{ "theme": "Rosé Pine" }\n' > "$XDG_CONFIG_HOME/hoja/settings.json"

# -u WAYLAND_DISPLAY or gpui picks the Wayland backend and lands on the real
# session.
env -u WAYLAND_DISPLAY DISPLAY="$DISP" "$BIN" "$START_DIR" >"$OUT/app.log" 2>&1 &
APP=$!
cleanup() { kill -9 "$APP" 2>/dev/null; kill "$XVFB" 2>/dev/null; }
trap cleanup EXIT

export DISPLAY=$DISP
for _ in $(seq 1 60); do
    WIN=$(xdotool search --name '^hoja$' 2>/dev/null | head -1)
    [ -n "$WIN" ] && break
    sleep 0.2
done
[ -z "${WIN:-}" ] && { echo "hoja never opened a window; see $OUT/app.log" >&2; exit 1; }

# There is no window manager on this display, so the window places itself.
xdotool windowmove "$WIN" 0 0
xdotool windowactivate "$WIN" 2>/dev/null
sleep 1.5
eval "$(xdotool getwindowgeometry --shell "$WIN")"

# shellcheck disable=SC2317  # sourced by the body, not called here
shot()  { import -window "$WIN" "$OUT/$1.png" 2>/dev/null; }
# shellcheck disable=SC2317
strip() { import -window "$WIN" -crop "700x26+0+$((HEIGHT - 26))" +repage "$OUT/$1.png" 2>/dev/null; }
# shellcheck disable=SC2317
row()   { echo $(( 74 + $1 * 22 )); }
# shellcheck disable=SC2317
click() { xdotool mousemove --window "$WIN" 150 "$(row "$1")" click 1; }
# shellcheck disable=SC2317
key()   { xdotool key --window "$WIN" "$@"; }

# shellcheck source=/dev/null
. "$BODY"

echo "$OUT"
