# A hover preview of main.rs, with its syntax colors.
. scripts/readme-shots/size.sh
size 1024 576
wait_for 'p[0]["row_count"] == 6'
hover 2
sleep 2.5
grim "$OUT/raw.png"
