# The command palette over the grid, filtered to the layout commands.
. scripts/readme-shots/size.sh
size 1024 576
wait_for 'p[0]["row_count"] == 24'
key -M ctrl -k 2 -m ctrl
sleep 5
key -M ctrl -M shift -k p -m shift -m ctrl
sleep 0.6
key view
sleep 0.8
grim "$OUT/raw.png"
