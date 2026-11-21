# The header: the project as a list on the left, the pictures as a grid on
# the right, the list's pane active. Run by run.sh through the harness.
. scripts/readme-shots/size.sh
size 1888 704
wait_for 'p[0]["row_count"] == 6'
key -k F3
sleep 0.8
key -M ctrl -k l -m ctrl
sleep 0.3
key -M ctrl -k a -m ctrl
key "$DEMO/Pictures"
key -k Return
sleep 0.8
key -M ctrl -k 2 -m ctrl
sleep 6
key -k Tab
sleep 1.5
grim "$OUT/raw.png"
