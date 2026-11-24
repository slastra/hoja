# `hoja --pick` for a folder.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-folder.sh

wait_for 'd["pick"] is not None and p[0]["row_count"] == 1'
expect 'p[0]["rows"] == ["sub"]' "a folder picker shows folders only"
expect 'd["pick"]["mode"] == "folder" and d["pick"]["accept"] == "Select"' "its accept button says Select"
expect 'd["pick"]["note"] == "hoja-picker"' "with nothing selected, it picks the folder shown"
shot picker-folder

click 0
wait_for 'd["pick"]["note"] == "sub"'
expect 'd["pick"]["note"] == "sub"' "a selected folder is the one it picks"

dbl 0
wait_for 'p[0]["dir"].endswith("/sub")'
expect 'p[0]["row_count"] == 0' "the folder's files do not show"
at $((WW - 30)) $((WH - 19))
STATUS=$(wait_exit)
python3 -I - "$OUT/app.out" "$START_DIR/sub" "$STATUS" <<'PY' && echo "  ok   the accept button answers with the folder" || { echo "  FAIL the accept button answers with the folder" >&2; FAILED=$((FAILED + 1)); }
import json, sys, urllib.parse
out, want, status = sys.argv[1:]
assert status == "0", status
answer = json.loads(open(out).read().strip().splitlines()[-1])
assert [urllib.parse.unquote(u) for u in answer["uris"]] == ["file://" + want], answer
PY
