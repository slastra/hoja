# The command palette runs a command from the footer's Run button and from a
# click on its row, and closes after each.
#
#   FIX=$(scripts/tests/setup-git.sh)
#   cargo build && scripts/sway-harness.sh "$FIX" scripts/tests/palette.sh
#
# "pane: reverse sort" is the command: its effect is in the probe's row order.
# Run is found in a screenshot, as the brightest text at the right of the
# modal, since nothing reports where the footer is drawn.

wait_for 'p[0]["row_count"] == 5'
# What heads the listing before the sort is reversed.
FIRST=$(python3 -c "import json;print(json.load(open('$HOJA_PROBE'))['panes'][0]['rows'][0])" 2>/dev/null)
key -M ctrl -M shift -k p -m shift -m ctrl
sleep 0.6
key reverse sort
sleep 0.5
shot palette-run
python3 -I - "$OUT/palette-run.png" <<'PY' > "$OUT/run-xy"
import sys
from PIL import Image
im=Image.open(sys.argv[1]).convert('RGB'); W,H=im.size
# The footer's "Run": the brightest text in the modal's bottom-right corner.
best=None
for y in range(80,600):
    for x in range(W//2+200, W//2+320):
        r,g,b=im.getpixel((x,y))
        if r+g+b>600: best=(x,y)
print(*best)
PY
read RX RY < "$OUT/run-xy"
goto "$((RX - 8))" "$RY"
wlrctl pointer click left >/dev/null 2>&1
wait_for 'p[0]["rows"][0] != "'"$FIRST"'"' 5
expect 'p[0]["rows"][0] != "'"$FIRST"'"' "Run reversed the sort"
key -M ctrl -M shift -k p -m shift -m ctrl
sleep 0.6
key reverse sort
sleep 0.5
# The one row: the modal's top at 80, its border, the 36 pixel query row and
# its rule, and the list's 4 pixels of padding put it at 122 to 154.
goto "$((WX + 520))" "$((WY + 133))"
wlrctl pointer click left >/dev/null 2>&1
wait_for 'p[0]["rows"][0] == "'"$FIRST"'"' 5
expect 'p[0]["rows"][0] == "'"$FIRST"'"' "a row click ran it again"
