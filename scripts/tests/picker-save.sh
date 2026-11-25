# `hoja --pick` for a save of one file.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-save.sh
#
# picker-save.before.sh gives the request on stdin. The fixture holds sub/,
# bundle.zip, notes.txt, part.stl and photo.png.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 2'
echo "  info the window took ${MAPPED_MS} ms to appear"
expect 'd["pick"]["mode"] == "save" and d["pick"]["name"] == "report.txt"' "the name field has the name the request gave"
expect 'd["pick"]["accept"] == "Save" and d["pick"]["note"].startswith("Saves in ")' "the accept button saves in the folder shown"
expect 'p[0]["rows"] == ["sub", "notes.txt"]' "the filter shows the files of its type"
shot picker-save

# A file selected gives its name, and saving over it asks first.
click "$(named notes.txt)"
wait_for 'd["pick"]["name"] == "notes.txt"'
expect 'd["pick"]["note"].startswith("Replaces a file in ")' "a file selected gives the field its name"
key -k Return
wait_for 'd["modal"] == "replace-prompt"'
expect 'd["modal"] == "replace-prompt"' "saving over a file asks first"
shot picker-save-replace
key -k Escape
wait_for 'd["modal"] is None'
if [ -s "$OUT/app.status" ]; then
    echo "  FAIL Cancel in the question goes back to the dialog" >&2; FAILED=$((FAILED + 1))
else
    echo "  ok   Cancel in the question goes back to the dialog"
fi

# The keyboard is back in the name field. A folder typed there goes into it,
# and the name goes back to the one the save started with.
key -M ctrl -P a -m ctrl
key sub
wait_for 'd["pick"]["name"] == "sub"'
key -k Return
wait_for 'p[0]["dir"].endswith("/sub")'
expect 'd["pick"]["name"] == "report.txt"' "a folder typed in the field goes into it"

# Enter in the field saves.
key -k Return
STATUS=$(wait_exit)
python3 -I - "$OUT/app.out" "$START_DIR/sub/report.txt" "$STATUS" <<'PY2' && echo "  ok   enter in the name field answers with the new file and exits" || { echo "  FAIL enter in the name field answers with the new file and exits" >&2; FAILED=$((FAILED + 1)); }
import json, sys, urllib.parse
out, want, status = sys.argv[1:]
assert status == "0", status
answer = json.loads(open(out).read().strip().splitlines()[-1])
assert [urllib.parse.unquote(u) for u in answer["uris"]] == ["file://" + want], answer
assert answer["filter"] == 0, answer
PY2
