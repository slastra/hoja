# Git status as the colour of a name, as Zed's project panel has it, and
# the colours following what git does without the pane being touched.
#
#   FIX=$(scripts/tests/setup-git.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/git.sh

wait_for 'p[0]["row_count"] == 5'
status() { probe "p[0]['git'][p[0]['rows'].index('$1')]"; }
wait_for "p[0]['git'][p[0]['rows'].index('new.txt')] == 'added'" 20
expect "p[0]['git'][p[0]['rows'].index('src')] == 'modified'" "a change deep in a folder colours the folder"
expect "p[0]['git'][p[0]['rows'].index('new.txt')] == 'added'" "a new file is added"
expect "p[0]['git'][p[0]['rows'].index('target')] == 'ignored'" "an ignored folder is dimmed"
expect "p[0]['git'][p[0]['rows'].index('clean')] == ''" \
    "a tracked folder with only an ignored file in it is not"
expect "p[0]['git'][p[0]['rows'].index('kept.txt')] == ''" "a committed file is plain"
shot git

# A commit writes only to .git, which the listing never sees. The names
# follow anyway.
g() { git -C "$START_DIR" -c user.name=t -c user.email=t@t "$@"; }
g add new.txt src
g commit -q -m second
wait_for "p[0]['git'][p[0]['rows'].index('new.txt')] == ''" 20
expect "p[0]['git'][p[0]['rows'].index('new.txt')] == '' and p[0]['git'][p[0]['rows'].index('src')] == ''" \
    "a commit in a terminal clears the colours"

# And an edit of a file in the listing colours it again.
printf 'changed\n' >> "$START_DIR/kept.txt"
wait_for "p[0]['git'][p[0]['rows'].index('kept.txt')] == 'modified'" 20
expect "p[0]['git'][p[0]['rows'].index('kept.txt')] == 'modified'" "an edit shows as modified"
