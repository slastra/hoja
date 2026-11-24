# `hoja --pick` canceled with escape.
#
#   cargo build
#   scripts/sway-harness.sh "$(scripts/tests/setup-picker.sh)" scripts/tests/picker-cancel.sh

wait_for 'd["pick"] is not None and p[0]["row_count"] == 5'
expect 'd["pick"]["mode"] == "file" and d["pick"]["accept"] is None' "an empty request picks one file"

click "$(named photo.png)"
wait_for 'd["pick"]["note"] == "photo.png"'
# The first escape clears the selection and leaves the picker open.
key -k Escape
wait_for 'p[0]["selected"] == []'
if [ "$(wait_exit 1)" = running ]; then
    echo "  ok   the first escape clears the selection and stays open"
else
    echo "  FAIL the first escape clears the selection and stays open" >&2
    FAILED=$((FAILED + 1))
fi

# The second cancels: exit status 1 and nothing on stdout.
key -k Escape
STATUS=$(wait_exit)
if [ "$STATUS" = 1 ] && [ ! -s "$OUT/app.out" ]; then
    echo "  ok   the second escape cancels, with nothing on stdout"
else
    echo "  FAIL the second escape cancels, with nothing on stdout: status $STATUS" >&2
    FAILED=$((FAILED + 1))
fi
