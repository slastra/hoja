wait_for 'p[0]["row_count"] == 9'
expect 'p[0]["rows"][0] == "aurora"' "folders sort first"
expect 'p[0]["footer"] == "9 items · 4.9 GB"' "footer totals the listing"

# Folder sizes arrive in the background; wait for them rather than sleeping.
wait_for 'p[0]["counting"] == []' 15
expect 'all(s for s in p[0]["sizes"])' "every row has a size"
expect 'p[0]["sizes"][2] == "2.8 GB"' "Downloads counted recursively"

key -k Down
wait_for 'p[0]["cursor"] == 0'
key -M shift -P Down -m shift
key -M shift -P Down -m shift
wait_for 'p[0]["selected"] == [0, 1, 2]'
expect '"3 selected" in p[0]["footer"]' "footer follows the selection"

key -k F3
wait_for 'len(p) == 2' 
expect 'p[1]["active"] and not p[0]["active"]' "the new pane takes focus"
