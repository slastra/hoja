# Back and Forward return to where each listing was left, as a browser does.
#
#   FIX=$(scripts/tests/setup-history.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/history.sh

wait_for 'p[0]["row_count"] == 80'

# Far enough down that the listing has scrolled.
key -k End
wait_for 'p[0]["cursor"] == 79'
wait_for 'p[0]["top_row"] > 0'
TOP=$(probe 'p[0]["top_row"]')
SEL=$(probe 'p[0]["selected"]')

key -k Return
wait_for 'p[0]["dir"].endswith("/f79")'
expect 'p[0]["cursor"] is None and p[0]["top_row"] == 0' "a new folder opens at the top, nothing chosen"
key -k Down
wait_for 'p[0]["cursor"] == 0'

# The dedicated key, then alt-left: both are the same Back.
key -k XF86Back
wait_for 'p[0]["row_count"] == 80'
wait_for 'p[0]["cursor"] == 79'
expect "p[0]['top_row'] == $TOP" "back puts the scroll where it was"
expect "p[0]['selected'] == $SEL" "and the selection"

key -k XF86Forward
wait_for 'p[0]["dir"].endswith("/f79")'
wait_for 'p[0]["cursor"] == 0'
expect 'p[0]["selected"] == [0]' "forward returns to where that folder was left"

key -M alt -k Left -m alt
wait_for 'p[0]["row_count"] == 80'
wait_for 'p[0]["cursor"] == 79'
expect "p[0]['top_row'] == $TOP" "alt-left does the same"

# The top of the listing is a place too: nothing scrolled, first row chosen.
key -k Home
wait_for 'p[0]["cursor"] == 0 and p[0]["top_row"] == 0'
key -k Return
wait_for 'p[0]["dir"].endswith("/f00")'
key -M alt -k Left -m alt
wait_for 'p[0]["row_count"] == 80'
wait_for 'p[0]["cursor"] == 0'
expect 'p[0]["top_row"] == 0' "and back to the top stays at the top"
