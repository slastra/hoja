# `hoja --pick` for one file, with filters.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-open.sh
#
# picker-open.before.sh gives the request on stdin. The fixture holds sub/,
# bundle.zip, notes.txt, part.stl and photo.png.

wait_for 'd["pick"] is not None and p[0]["row_count"] == 2'
echo "  info the window took ${MAPPED_MS} ms to appear"
expect 'p[0]["rows"] == ["sub", "photo.png"]' "the filter the request asked for shows the folder and the picture only"
expect 'd["pick"]["mode"] == "file" and d["pick"]["filter"] == "Images"' "the bar names the filter"
expect 'd["pick"]["accept"] is None and d["pick"]["note"] == "Select a file"' "the accept button waits for a file"
shot picker-open

# One file only: shift-down moves the selection instead of growing it.
click "$(named sub)"
key -M shift -k Down -m shift
wait_for 'p[0]["cursor"] == 1'
expect 'p[0]["selected"] == [1]' "a picker for one file selects one row"
expect 'd["pick"]["accept"] == "Attach" and d["pick"]["note"] == "photo.png"' "the accept button has the request's label, without its mnemonic"

# A double-click on a folder goes into it, and backspace comes back.
dbl "$(named sub)"
wait_for 'p[0]["dir"].endswith("/sub") and p[0]["rows"] == ["deep.png"]'
expect 'p[0]["rows"] == ["deep.png"]' "a double-click on a folder goes into it, with the filter still on"
key -k BackSpace
wait_for 'p[0]["rows"] == ["sub", "photo.png"]'

# The other filter, from the bar's menu.
at $((WW - 40)) $((WH - 50))
wait_for 'p[0]["menu_open"]'
menu_to Text
key -k Return
wait_for 'p[0]["rows"] == ["sub", "notes.txt"]'
expect 'd["pick"]["filter"] == "Text"' "a filter chosen from the menu applies at once"
shot picker-open-text

# A double-click on a file answers with it, and the filter it was found by.
dbl "$(named notes.txt)"
STATUS=$(wait_exit)
python3 -I - "$OUT/app.out" "$START_DIR/notes.txt" "$STATUS" <<'PY2' && echo "  ok   a double-click on a file answers with it and exits" || { echo "  FAIL a double-click on a file answers with it and exits" >&2; FAILED=$((FAILED + 1)); }
import json, sys, urllib.parse
out, want, status = sys.argv[1:]
assert status == "0", status
answer = json.loads(open(out).read().strip().splitlines()[-1])
assert [urllib.parse.unquote(u) for u in answer["uris"]] == ["file://" + want], answer
assert answer["filter"] == 0, answer
PY2
