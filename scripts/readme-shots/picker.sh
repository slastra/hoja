# Hoja as a save dialog: the name field and the footer under the grid. Run
# by run.sh through the harness.
. scripts/readme-shots/size.sh
size 1024 640
wait_for 'd["pick"] is not None and p[0]["row_count"] == 24'
sleep 6
grim "$OUT/raw.png"
