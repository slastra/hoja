# Dragging tabs: along a bar, to another pane's bar, onto another pane.
#
#   FIX=$(scripts/tests/setup-tabs.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/tab-drag.sh

wait_for 'p[0]["row_count"] == 2'
ROOT=$(probe 'd["tabs"][0]["titles"][0]')
expect 'not d["tabs"][0]["bar"]' "one pane with one tab has no bar"

# In a split every pane shows its bar, so a lone tab can be grabbed.
key -k F3
wait_for 'len(p) == 2 and p[1]["active"] and len(d["tabs"][1]["rects"]) == 1'
expect 'all(t["bar"] for t in d["tabs"])' "a split shows a bar on each pane"
key -k Down
wait_for 'p[1]["cursor"] == 0'
key -k Return
wait_for 'p[1]["dir"].endswith("/alpha")'

# A tab dropped on another pane's tab joins that pane, where it was dropped,
# and the pane it left, now empty, closes.
# shellcheck disable=SC2046  # two words each, x and y
drag $(tabat 0 0 1) $(tabat 0)
wait_for 'len(p) == 1 and len(d["tabs"][0]["titles"]) == 2'
expect "d['tabs'][0]['titles'] == ['alpha', '$ROOT']" "a dropped tab goes where it was dropped"
expect 'd["tabs"][0]["active"] == 0 and p[0]["dir"].endswith("/alpha")' "and it is the one shown"
expect 'p[0]["active"]' "and its pane has the keys"
expect 'd["hidden_watching"] == 0' "the tab now hidden watches nothing"

# Along its own bar, a tab changes places and stays shown.
wait_for 'len(d["tabs"][0]["rects"]) == 2'
# shellcheck disable=SC2046
drag $(tabat 0) $(tabat 1)
wait_for "d['tabs'][0]['titles'] == ['$ROOT', 'alpha']"
expect 'd["tabs"][0]["active"] == 1 and p[0]["dir"].endswith("/alpha")' "a tab moved along the bar stays shown"

# Onto another pane's body: the tab goes after that pane's shown tab.
key -k F3
wait_for 'len(p) == 2 and p[1]["active"] and len(d["tabs"][1]["rects"]) == 1'
# shellcheck disable=SC2046
drag $(tabat 0) $((WW * 3 / 4)) $((WH / 2))
wait_for 'len(d["tabs"][1]["titles"]) == 2'
expect "d['tabs'][1]['titles'] == ['alpha', '$ROOT'] and d['tabs'][1]['active'] == 1" "a tab dropped on a pane goes after its shown tab"
expect "d['tabs'][0]['titles'] == ['alpha'] and len(p) == 2" "the pane it left keeps its other tab"
expect 'p[1]["active"] and not p[0]["active"]' "the pane it went to has the keys"

# Its last tab dropped on another pane, a pane closes.
wait_for 'len(d["tabs"][0]["rects"]) == 1'
# shellcheck disable=SC2046
drag $(tabat 0) $((WW * 3 / 4)) $((WH / 2))
wait_for 'len(p) == 1'
expect "len(d['tabs'][0]['titles']) == 3" "a pane whose last tab leaves closes"
expect 'd["tabs"][0]["bar"]' "the bar stays while there are tabs to show"
expect 'd["hidden_watching"] == 0' "hidden tabs still watch nothing"

# A drop on the pane a tab is already in leaves it where it is.
# shellcheck disable=SC2046
drag $(tabat 2) $((WW / 2)) $((WH / 2))
sleep 0.5
expect 'len(d["tabs"][0]["titles"]) == 3 and d["tabs"][0]["active"] == 2' "a tab dropped on its own pane stays"

# --- splitting at an edge ----------------------------------------------------

# slow_drag X1 Y1 X2 Y2: a drag in window coordinates, left running in the
# background with the button held at the end, so a check can read the
# highlight before the drop. `wait "$DRAG"` lets it finish.
slow_drag() {
    drag_at "$((WX + $1))" "$((WY + $2))" "$((WX + $3))" "$((WY + $4))" --hold-ms 1500 &
    DRAG=$!
}
# home: drop the tab of the second pane at the end of the first pane's bar,
# which leaves one pane again.
home() {
    local end
    end=$(probe "f'{d[\"tabs\"][0][\"rects\"][-1][0] + d[\"tabs\"][0][\"rects\"][-1][2] + 40} {d[\"tabs\"][0][\"rects\"][-1][1] + 16}'")
    # shellcheck disable=SC2046,SC2086
    drag $(tabat 0 0 1) $end
    wait_for 'len(p) == 1'
}

# Back to two tabs in one pane: the root, and alpha shown.
for _ in 1 2 3 4; do
    [ "$(probe 'len(d["tabs"][0]["titles"]) > 1')" = "True" ] || break
    key -M ctrl -k w -m ctrl
    sleep 0.3
done
wait_for 'len(d["tabs"][0]["titles"]) == 1'
key -M alt -k Up -m alt
wait_for "d['tabs'][0]['titles'] == ['$ROOT']"
key -M ctrl -k t -m ctrl
wait_for 'len(d["tabs"][0]["rects"]) == 2'
goto "$((WX + 150))" "$(row "$(named alpha)")"
key -k Home
wait_for 'p[0]["cursor"] == 0'
key -k Return
wait_for 'p[0]["dir"].endswith("/alpha")'

# Near the right edge, the right half lights, and the drop splits it off there.
# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" $((WW - 10)) $((WH / 2))
wait_for 'd["tabs"][0]["drag_split"] == "right"'
shot drag-right
wait "$DRAG"
wait_for 'len(p) == 2'
expect "d['tabs'][0]['titles'] == ['$ROOT'] and d['tabs'][1]['titles'] == ['alpha']" "a tab dropped at an edge gets a pane of its own"
expect 'd["tabs"][1]["rects"][0][0] > d["tabs"][0]["rects"][0][0] + 100' "on the side it was dropped"
expect 'p[1]["active"] and p[1]["dir"].endswith("/alpha")' "and that pane has the keys"
expect 'd["tabs"][0]["drag_split"] is None and d["tabs"][1]["drag_split"] is None' "the highlight goes with the drop"
home

# The other three sides.
# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" 10 $((WH / 2))
wait_for 'd["tabs"][0]["drag_split"] == "left"'
shot drag-left
wait "$DRAG"
wait_for 'len(p) == 2'
expect 'd["tabs"][1]["rects"][0][0] + 100 < d["tabs"][0]["rects"][0][0]' "a drop at the left edge splits to the left"
home

# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" $((WW / 2)) $((WH - 10))
wait_for 'd["tabs"][0]["drag_split"] == "down"'
shot drag-down
wait "$DRAG"
wait_for 'len(p) == 2'
expect 'd["tabs"][1]["rects"][0][1] > d["tabs"][0]["rects"][0][1] + 100' "a drop at the bottom edge splits below"
home

# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" $((WW / 2)) 45
wait_for 'd["tabs"][0]["drag_split"] == "up"'
shot drag-up
wait "$DRAG"
wait_for 'len(p) == 2'
expect 'd["tabs"][1]["rects"][0][1] + 100 < d["tabs"][0]["rects"][0][1]' "a drop at the top edge splits above"
home
expect 'd["hidden_watching"] == 0' "after all that, hidden tabs watch nothing"

# In the middle, the whole pane lights and nothing splits.
# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" $((WW / 2)) $((WH / 2))
# Once the drag is on, the time the pointer takes to get there.
wait_for 'd["dragging"]'
sleep 0.8
shot drag-center
expect 'd["dragging"] and d["tabs"][0]["drag_split"] is None' "in the middle of a pane, no side is lit"
wait "$DRAG"
sleep 0.5
expect 'len(p) == 1 and len(d["tabs"][0]["titles"]) == 2' "and a drop there on its own pane changes nothing"

# With ctrl held at the drop, the tab stays and a copy splits off.
# shellcheck disable=SC2046
set -- $(tabat 1)
slow_drag "$1" "$2" $((WW - 10)) $((WH / 2))
wait_for 'd["tabs"][0]["drag_split"] == "right"'
wtype -M ctrl -s 4000 -m ctrl &
CTRL=$!
wait "$DRAG"
wait "$CTRL"
wait_for 'len(p) == 2'
expect 'len(d["tabs"][0]["titles"]) == 2 and d["tabs"][1]["titles"] == ["alpha"]' "a ctrl drop copies the tab"

# A lone tab over its own pane's edge has nowhere to split off to.
# shellcheck disable=SC2046
set -- $(tabat 0 0 1)
slow_drag "$1" "$2" $((WW - 10)) $((WH / 2))
wait_for 'd["dragging"]'
sleep 0.8
expect 'd["dragging"] and d["tabs"][1]["drag_split"] is None' "a lone tab does not split from its own pane"
wait "$DRAG"
sleep 0.5
expect 'len(p) == 2' "and dropping it there changes nothing"
