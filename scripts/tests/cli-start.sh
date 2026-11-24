# Several paths on the command line open as tabs, and a file opens its folder
# with the file selected.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-tabs.sh)" scripts/tests/cli-start.sh

wait_for 'len(d["tabs"][0]["titles"]) == 3 and p[0]["row_count"] == 1'
expect 'd["tabs"][0]["titles"][:2] == ["beta", "alpha"]' "each path is a tab, in order"
expect 'd["tabs"][0]["active"] == 0' "the first path is the tab shown"
expect "p[0]['dir'] == '$START_DIR/beta'" "a file opens its folder"
wait_for 'p[0]["selected"] == [0]'
expect 'p[0]["rows"][p[0]["cursor"]] == "two.txt"' "the file is selected"
expect 'd["hidden_watching"] == 0' "the hidden tabs watch nothing"

key -M ctrl -k Tab -m ctrl
wait_for 'd["tabs"][0]["active"] == 1'
expect "p[0]['dir'] == '$START_DIR/alpha'" "the second tab shows the second file's folder"
wait_for 'p[0]["selected"] == [0]'
expect 'p[0]["rows"][p[0]["cursor"]] == "one.txt"' "a tab first shown later still selects its file"

key -M ctrl -k Tab -m ctrl
wait_for 'd["tabs"][0]["active"] == 2'
expect "p[0]['dir'] == '$START_DIR'" "the last tab shows the folder given"
expect 'p[0]["selected"] == []' "a folder opens with nothing selected"
