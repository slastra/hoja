# Dragging files between hoja and another application: out to nemo, and in
# from it. Both directions are Wayland data-device drags between two clients,
# which no amount of testing inside one app can stand in for.
#
#   FIX=$(scripts/tests/setup-drag-external.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/drag-external.sh
#
# Needs nemo. It runs in the nested sway beside hoja, with three things kept
# away from the session it is nested in: a private D-Bus, so it cannot hand
# the window to a nemo already running; a keyfile settings backend under the
# test's own config, so it never writes dconf; and only the nested display.
#
# Coordinates come from the layout sway gives two windows side by side, read
# back rather than assumed, and from nemo's list view, whose first row sits
# just under its header.

command -v nemo >/dev/null || { echo "  skip nemo is not installed"; return 0; }
NEMO_DIR="$(dirname "$START_DIR")/nemo-side"

mkdir -p "$XDG_CONFIG_HOME/glib-2.0/settings"
cat > "$XDG_CONFIG_HOME/glib-2.0/settings/keyfile" <<'KEY'
[org/nemo/preferences]
default-folder-viewer='list-view'
[org/nemo/window-state]
start-with-sidebar=false
start-with-status-bar=false
KEY
GSETTINGS_BACKEND=keyfile GDK_BACKEND=wayland NO_AT_BRIDGE=1 \
    dbus-run-session -- nemo --no-desktop "$NEMO_DIR" >"$OUT/nemo.log" 2>&1 &

# rect APP_ID: "x y w h" of that window, once it is mapped.
rect() {
    swaymsg -t get_tree 2>/dev/null | python3 -c "
import sys, json
def walk(n):
    if (n.get('app_id') or '').startswith(sys.argv[1]):
        r = n['rect']; print(r['x'], r['y'], r['width'], r['height']); sys.exit()
    for k in ('nodes', 'floating_nodes'):
        for c in n.get(k, []): walk(c)
walk(json.load(sys.stdin))" "$1"
}
for _ in $(seq 1 80); do
    [ -n "$(rect nemo)" ] && break
    sleep 0.25
done
[ -n "$(rect nemo)" ] || { echo "  FAIL nemo never mapped; see $OUT/nemo.log"; FAILED=$((FAILED + 1)); return 0; }
sleep 1.5   # its first listing
read -r HX HY HW HH <<<"$(rect hoja)"
read -r NX NY NW NH <<<"$(rect nemo)"
wait_for 'p[0]["rows"] == ["outbound.txt", "pack.zip"]'

# --- out ------------------------------------------------------------------
# From hoja's first row to the empty middle of nemo's listing.
drag_at $((HX + 150)) $((HY + 63)) $((NX + NW / 2)) $((NY + NH / 2))
for _ in $(seq 1 50); do
    [ -e "$NEMO_DIR/outbound.txt" ] && break
    sleep 0.2
done
if [ -e "$NEMO_DIR/outbound.txt" ] && [ "$(cat "$NEMO_DIR/outbound.txt")" = out ]; then
    echo "  ok   a file dragged out of hoja lands in nemo"
else
    echo "  FAIL a file dragged out of hoja never reached nemo" >&2
    FAILED=$((FAILED + 1))
fi
grim "$OUT/after-out.png"

# --- in -------------------------------------------------------------------
# nemo's first list row, onto the empty lower half of hoja's pane.
drag_at $((NX + 60)) $((NY + 110)) $((HX + HW / 2)) $((HY + HH * 3 / 4))
wait_for '"inbound.txt" in p[0]["rows"]' 15
expect '"inbound.txt" in p[0]["rows"]' "a file dragged in from nemo lands in hoja"
for _ in $(seq 1 50); do
    [ -e "$START_DIR/inbound.txt" ] && break
    sleep 0.2
done
[ "$(cat "$START_DIR/inbound.txt" 2>/dev/null)" = in ] &&
    echo "  ok   with its contents" || { echo "  FAIL contents differ" >&2; FAILED=$((FAILED + 1)); }
grim "$OUT/after-in.png"

# --- out of an archive ------------------------------------------------------
# The member does not exist as a file until a target asks for it. hoja hands
# its own window the archive and everybody after it the extracted member
# (see `Payload::Lazy`), and nemo asks during the motion, so getting the order
# wrong delivers the whole zip.
dbl "$(named pack.zip)"
wait_for 'p[0]["rows"] == ["member.txt"]'
drag_at $((HX + 150)) $((HY + 63)) $((NX + NW / 2)) $((NY + NH / 2))
for _ in $(seq 1 50); do
    [ -e "$NEMO_DIR/member.txt" ] && break
    sleep 0.2
done
if [ "$(cat "$NEMO_DIR/member.txt" 2>/dev/null)" = "from inside" ]; then
    echo "  ok   a member dragged out of an archive lands as itself"
else
    echo "  FAIL the archive member never reached nemo" >&2
    FAILED=$((FAILED + 1))
fi
if [ -e "$NEMO_DIR/pack.zip" ]; then
    echo "  FAIL nemo was handed the whole archive" >&2
    FAILED=$((FAILED + 1))
else
    echo "  ok   and not the archive around it"
fi
