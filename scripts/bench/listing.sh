# How long from asking for a large directory to seeing all of it.
#
#   ./scripts/bench/setup-100k.sh
#   HOJA_BIN=./target/release/hoja \
#     ./scripts/sway-harness.sh ~/.cache/hoja-bench scripts/bench/listing.sh
#
# Measured from the double-click to the moment the app publishes a probe saying
# every row is listed, which is its own write timestamp rather than the poll
# that noticed it: polling spawns a python process, and that is tens of
# milliseconds of floor on a number in the low hundreds.
#
# Release, or the figure is meaningless: the sort alone is an order of magnitude
# slower in debug.
wait_for 'p[0]["row_count"] == 1'

run() {
    local start end
    start=$(date +%s.%N)
    dbl 0
    wait_for 'p[0]["row_count"] == 100000' 60 || { echo "  timeout"; return; }
    end=$(stat -c %.9Y "$HOJA_PROBE")
    printf '  %7.0f ms\n' "$(echo "($end - $start) * 1000" | bc)"
    key -k BackSpace
    wait_for 'p[0]["row_count"] == 1' 30
    sleep 0.3
}

echo "double-click into a directory of 100,000 files, to fully listed:"
for _ in 1 2 3 4 5; do run; done
