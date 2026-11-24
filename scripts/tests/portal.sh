# The whole loop: an app asks hoja-portal for files over D-Bus, the portal
# starts `hoja --pick`, and the files picked come back in the reply.
#
#   cargo build --workspace
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/portal.sh
#
# portal.before.sh starts a private bus with the portal on it, and runs a
# `gdbus call` in place of the app. Its reply is the app's stdout, app.out.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 5'
echo "  info the window took ${MAPPED_MS} ms to appear"
expect 'd["pick"]["mode"] == "files"' "the portal's request reaches Hoja"
expect "p[0][\"dir\"] == \"$START_DIR\"" "Hoja opens in the folder the app asked for"

click "$(named notes.txt)"
key -M shift -k Down -m shift
wait_for 'd["pick"]["note"] == "2 files"'
key -k Return
STATUS=$(wait_exit 10)
WANT="(uint32 0, {'uris': <['file://$START_DIR/notes.txt', 'file://$START_DIR/part.stl']>})"
if [ "$STATUS" = 0 ] && [ "$(cat "$OUT/app.out")" = "$WANT" ]; then
    echo "  ok   the app's reply holds the two files"
else
    echo "  FAIL the app's reply holds the two files: status $STATUS" >&2
    echo "       $(cat "$OUT/app.out")" >&2
    FAILED=$((FAILED + 1))
fi
