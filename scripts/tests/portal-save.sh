# A save of several files through the whole loop: an app asks hoja-portal
# over D-Bus, the portal starts `hoja --pick`, and the paths come back in the
# reply.
#
#   cargo build --workspace
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/portal-save.sh
#
# portal-save.before.sh starts a private bus with the portal on it, and runs a
# `gdbus call` in place of the app. Its reply is the app's stdout, app.out.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 1'
expect 'd["pick"]["mode"] == "save-files" and p[0]["rows"] == ["sub"]' "a save of several files picks a folder, and shows folders only"
expect 'd["pick"]["note"].endswith(", 1 of them over files there")' "the bar says one file is there already"

# Saving over notes.txt asks first. Enter takes Replace.
key -k Return
wait_for 'd["modal"] == "replace-prompt"'
key -k Return
STATUS=$(wait_exit 10)
WANT="(uint32 0, {'uris': <['file://$START_DIR/notes.txt', 'file://$START_DIR/new.txt']>})"
if [ "$STATUS" = 0 ] && [ "$(cat "$OUT/app.out")" = "$WANT" ]; then
    echo "  ok   the app's reply holds both names in the folder"
else
    echo "  FAIL the app's reply holds both names in the folder: status $STATUS" >&2
    echo "       $(cat "$OUT/app.out")" >&2
    FAILED=$((FAILED + 1))
fi
