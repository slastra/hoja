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
