# A private D-Bus with hoja-portal on it, and in place of the app, a stand-in
# that asks the portal where to save two files, as xdg-desktop-portal would.
# See portal-save.sh.
#
# The bus starts nothing on its own: it has no service dirs. A bus that could
# start services started a second document portal once, which unmounted the
# live session's (see drag-external.sh).
cat >"$OUT/portal-bus.conf" <<'BUS'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
BUS
dbus-daemon --config-file="$OUT/portal-bus.conf" --fork \
    --print-address=3 --print-pid=4 3>"$OUT/bus.address" 4>"$OUT/bus.pid"
PORTAL_BUS=$(head -1 "$OUT/bus.address")
export PORTAL_BUS
EXTRA_PIDS="$(cat "$OUT/bus.pid")"

DBUS_SESSION_BUS_ADDRESS=$PORTAL_BUS HOJA_PORTAL_HOJA="$PWD/target/debug/hoja" \
    "$PWD/target/debug/hoja-portal" 2>"$OUT/portal.log" &
EXTRA_PIDS="$EXTRA_PIDS $!"
for _ in $(seq 1 50); do
    busctl --address="$PORTAL_BUS" list 2>/dev/null \
        | grep -q org.freedesktop.impl.portal.desktop.hoja && break
    sleep 0.1
done

# The folder goes as a list of bytes. gdbus's own `b'...'` spelling for one
# sent other bytes of the right length (GLib 2.90).
cat >"$OUT/app.sh" <<'APP'
#!/bin/sh
# The app: asks where to save notes.txt and new.txt, from the folder it is
# given.
bytes() { python3 -I -c 'import sys; print(", ".join(map(str, sys.argv[1].encode() + b"\0")))' "$1"; }
exec gdbus call --address "$PORTAL_BUS" --timeout 60 \
    --dest org.freedesktop.impl.portal.desktop.hoja \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.impl.portal.FileChooser.SaveFiles \
    /org/freedesktop/portal/desktop/request/1_1/test test.app "" "Save attachments" \
    "{'current_folder': <@ay [$(bytes "$1")]>, 'files': <@aay [[$(bytes notes.txt)], [$(bytes new.txt)]]>}"
APP
chmod +x "$OUT/app.sh"
BIN="$OUT/app.sh"
APP_ID=hoja-picker
