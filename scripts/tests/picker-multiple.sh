# `hoja --pick` for several files. An archive is a file to pick: the picker
# never goes inside one, where nothing has a path to give.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-multiple.sh
#
# The fixture holds sub/, bundle.zip, notes.txt, part.stl and photo.png.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 5'
expect 'd["pick"]["mode"] == "files" and d["pick"]["filter"] is None' "a request with no filters shows every file"

# Two files by a range, then the accept button.
click "$(named bundle.zip)"
key -M shift -k Down -m shift
wait_for 'd["pick"]["note"] == "2 files"'
expect 'd["pick"]["accept"] == "Open"' "two files selected can be picked"
shot picker-multiple
at $((WW - 30)) $((WH - 19))
STATUS=$(wait_exit)
python3 -I - "$OUT/app.out" "$START_DIR" "$STATUS" <<'PY' && echo "  ok   the accept button answers with both, the archive as a file" || { echo "  FAIL the accept button answers with both, the archive as a file" >&2; FAILED=$((FAILED + 1)); }
import json, sys, urllib.parse
out, start, status = sys.argv[1:]
assert status == "0", status
answer = json.loads(open(out).read().strip().splitlines()[-1])
got = [urllib.parse.unquote(u) for u in answer["uris"]]
assert got == [f"file://{start}/bundle.zip", f"file://{start}/notes.txt"], got
assert answer["filter"] is None, answer
PY
