# Tabs: several folders in one pane, one of them shown.
#
#   FIX=$(scripts/tests/setup-tabs.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/tabs.sh

wait_for 'p[0]["row_count"] == 2'
ROOT=$(probe 'p[0]["dir"]')

key -M ctrl -k t -m ctrl
wait_for 'len(d["tabs"][0]["titles"]) == 2 and d["tabs"][0]["active"] == 1'
expect "p[0]['dir'] == '$ROOT'" "a new tab opens where the last one was"
expect 'len(p) == 1' "a tab is not a pane"
expect 'd["hidden_watching"] == 0' "the hidden tab watches nothing"

# Somewhere else in the new tab, and the first tab stays where it was.
key -k Down
wait_for 'p[0]["cursor"] == 0'
key -k Return
wait_for 'p[0]["dir"].endswith("/alpha")'
expect 'd["tabs"][0]["titles"][1] == "alpha"' "a tab is named for its folder"
key -M ctrl -k Page_Up -m ctrl
wait_for 'd["tabs"][0]["active"] == 0'
expect "p[0]['dir'] == '$ROOT'" "the first tab kept its folder"
expect 'd["hidden_watching"] == 0' "the tab now hidden stopped watching"

# Written while its tab is hidden, so no watch sees it: showing the tab must.
: > "$START_DIR/alpha/new.txt"
sleep 1
key -M alt -k 2 -m alt
wait_for 'd["tabs"][0]["active"] == 1'
wait_for '"new.txt" in p[0]["rows"]'
expect '"new.txt" in p[0]["rows"]' "a tab shown again reads its folder"

key -M ctrl -k Tab -m ctrl
wait_for 'd["tabs"][0]["active"] == 0'
expect 'd["tabs"][0]["active"] == 0' "ctrl-tab wraps to the first tab"

# A third, then the ends by number: nine is the last however many there are.
key -M ctrl -k t -m ctrl
wait_for 'len(d["tabs"][0]["titles"]) == 3 and d["tabs"][0]["active"] == 1'
expect 'd["tabs"][0]["titles"][2] == "alpha"' "a new tab goes after the shown one"
key -M alt -k 9 -m alt
wait_for 'd["tabs"][0]["active"] == 2'
key -M alt -k 1 -m alt
wait_for 'd["tabs"][0]["active"] == 0'
expect 'd["hidden_watching"] == 0' "two hidden tabs watch nothing"

# Tab between panes passes over the hidden tabs: two panes, two stops.
key -k F3
wait_for 'len(p) == 2 and p[1]["active"]'
key -k Tab
wait_for 'p[0]["active"]'
key -k Tab
wait_for 'p[1]["active"]'
expect 'p[1]["active"] and not p[0]["active"]' "tab steps over hidden tabs"

# ctrl-w closes a tab, and with the last one its pane.
key -M ctrl -k w -m ctrl
wait_for 'len(p) == 1 and p[0]["active"]'
expect 'len(d["tabs"][0]["titles"]) == 3' "closing a one-tab pane leaves the other's tabs"
key -M ctrl -k w -m ctrl
wait_for 'len(d["tabs"][0]["titles"]) == 2'
expect 'd["tabs"][0]["active"] == 0' "the next tab takes the closed one's place"
expect "p[0]['dir'] == '$ROOT'" "and shows its own folder"
key -M ctrl -k w -m ctrl
wait_for 'len(d["tabs"][0]["titles"]) == 1'
key -M ctrl -k w -m ctrl
sleep 0.5
expect 'len(p) == 1 and len(d["tabs"][0]["titles"]) == 1' "the last tab of the last pane stays"

# The bar: none with one tab, and the rows where they always were.
expect 'd["tabs"][0]["rects"] == []' "one tab has no bar"
Y0=$(row 0)
key -M ctrl -k t -m ctrl
wait_for 'len(d["tabs"][0]["rects"]) == 2'
expect "d['tabs'][0]['rects'][0][3] == 32" "the bar is Zed's height"
shot two-tabs
expect "$(row 0) == $Y0 + 32" "the rows move down under the bar"

tab 0
wait_for 'd["tabs"][0]["active"] == 0'
expect 'd["tabs"][0]["active"] == 0' "a click shows a tab"

mtab 1
wait_for 'len(d["tabs"][0]["titles"]) == 1'
expect 'd["tabs"][0]["rects"] == []' "a middle click closes a tab, and the bar goes with the second"
expect "$(row 0) == $Y0" "the rows go back up"

# A double click on the empty end of the bar opens a tab. The ctrl-t first
# is a check too: a press on the bar once gave the keys to the window, and
# after a middle click nothing gave them back.
key -M ctrl -k t -m ctrl
wait_for 'len(d["tabs"][0]["rects"]) == 2'
goto "$((WX + WW - 40))" "$((WY + 16))"
wlrctl pointer click left >/dev/null 2>&1
wlrctl pointer click left >/dev/null 2>&1
wait_for 'len(d["tabs"][0]["titles"]) == 3'
expect 'd["tabs"][0]["active"] == 2' "a double click on the bar opens a tab"

tabclose 0
wait_for 'len(d["tabs"][0]["titles"]) == 2'
expect 'd["tabs"][0]["active"] == 1' "the close button closes its own tab, not the shown one"

# Back to one tab at the top of the fixture.
for _ in 1 2 3 4; do
    [ "$(probe 'len(d["tabs"][0]["titles"]) > 1')" = "True" ] || break
    key -M ctrl -k w -m ctrl
    sleep 0.3
done
wait_for 'len(d["tabs"][0]["titles"]) == 1'
key -M alt -k Up -m alt
wait_for "p[0]['dir'] == '$ROOT' and p[0]['rows'] == ['alpha', 'beta']"

# A middle click on a folder opens it in a tab and stays where it is.
goto "$((WX + 150))" "$(row "$(named beta)")"
wlrctl pointer click middle >/dev/null 2>&1
wait_for 'len(d["tabs"][0]["titles"]) == 2'
expect 'd["tabs"][0]["titles"][1] == "beta"' "a middle click opens a folder in a tab"
expect "d['tabs'][0]['active'] == 0 and p[0]['dir'] == '$ROOT'" "and the shown tab stays"
expect 'd["hidden_watching"] == 0' "the new tab, hidden, watches nothing"

# A file dropped on a tab goes into that tab's folder.
dbl "$(named alpha)"
wait_for 'p[0]["dir"].endswith("/alpha") and "one.txt" in p[0]["rows"]'
FROM_Y=$(( $(row "$(named one.txt)") - WY ))
# shellcheck disable=SC2046  # two words, x and y
drag 150 "$FROM_Y" $(tabat 1)
wait_for '"one.txt" not in p[0]["rows"]'
expect "$([ -e "$START_DIR/beta/one.txt" ] && echo True || echo False)" "a file dropped on a tab lands in its folder"
tab 1
wait_for '"one.txt" in p[0]["rows"]'
expect 'p[0]["dir"].endswith("/beta")' "and the tab shows it there"
