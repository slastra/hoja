# Open Folder in a picker that was asked for a file, as Zed's ctrl-o asks.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-open-folder.sh
#
# The fixture holds sub/, bundle.zip, notes.txt, part.stl and photo.png.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 5'
NAME=$(basename "$START_DIR")
expect "d['pick']['folder'] == '$NAME'" "with nothing selected, Open Folder is the folder shown"
expect 'd["pick"]["accept"] is None' "and Open has nothing to open"

click "$(named notes.txt)"
wait_for 'd["pick"]["note"] == "notes.txt"'
expect "d['pick']['folder'] == '$NAME'" "a file selected leaves Open Folder on the folder shown"

click "$(named sub)"
wait_for 'd["pick"]["note"] == "Open sub"'
expect 'd["pick"]["folder"] == "sub"' "a folder selected is the one Open Folder picks"
expect 'd["pick"]["accept"] == "Open"' "and Open still goes into it"
shot picker-open-folder

key -M ctrl -k Return -m ctrl
STATUS=$(wait_exit)
python3 -I - "$OUT/app.out" "$START_DIR/sub" "$STATUS" <<'PY' && echo "  ok   ctrl-enter answers with the folder" || { echo "  FAIL ctrl-enter answers with the folder" >&2; FAILED=$((FAILED + 1)); }
import json, sys, urllib.parse
out, want, status = sys.argv[1:]
assert status == "0", status
answer = json.loads(open(out).read().strip().splitlines()[-1])
assert [urllib.parse.unquote(u) for u in answer["uris"]] == ["file://" + want], answer
PY
