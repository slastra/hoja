# Hover previews: the pointer rests on a row and a tooltip shows the file.
#
#   FIX=$(scripts/tests/setup-preview.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/preview.sh
#
# Needs ffmpeg for the fixture and the system's thumbnailers for the pictures.
# The harness points XDG_CACHE_HOME into the out-dir, so the thumbnails made
# here never reach the shared cache.

wait_for 'p[0]["row_count"] == 5'

# One row at a time. The probe names the kind once the work is done.
look() {
    local name=$1 kind=$2
    hover "$(named "$name")"
    wait_for "d['preview'] == '$kind: $name'" 20
    expect "d['preview'] == '$kind: $name'" "$name shows a $kind preview"
    shot "preview-${name%%.*}"
}

look photo.png image
look clip.mp4 video
look tone.ogg audio
look notes.rs text

# A picture is in the cache now, under the spec's name, for the image and the
# video at least. Audio without embedded art may have none.
THUMBS=$(find "$XDG_CACHE_HOME/thumbnails" -name '*.png' -not -name '.*' -path '*large*' | wc -l)
if [ "$THUMBS" -ge 2 ]; then
    echo "  ok   pictures went into the cache ($THUMBS)"
else
    echo "  FAIL pictures went into the cache: found $THUMBS" >&2
    FAILED=$((FAILED + 1))
fi

# A file with nothing to show: the kind is known and nothing is drawn.
hover "$(named blob.bin)"
wait_for "d['preview'] == 'none: blob.bin'" 10
expect "d['preview'] == 'none: blob.bin'" "a binary file has no preview"

# A click closes the tooltip.
look photo.png image
click "$(named photo.png)"
wait_for 'd["preview"] is None' 5
expect 'd["preview"] is None' "a click closes the preview"

# The second look at a file comes from the cache: the tooltip opens with the
# preview in it, and the probe never says `loading` for it.
hover "$(named clip.mp4)"
SEEN=""
for _ in $(seq 1 40); do
    NOW=$(probe 'd["preview"]')
    [ "$NOW" != "None" ] && SEEN="$SEEN|$NOW"
    [ "$NOW" = "video: clip.mp4" ] && break
    sleep 0.05
done
case "$SEEN" in
    *loading*) echo "  FAIL a second look comes from the cache: $SEEN" >&2; FAILED=$((FAILED + 1)) ;;
    *"video: clip.mp4") echo "  ok   a second look comes from the cache" ;;
    *) echo "  FAIL a second look comes from the cache: $SEEN" >&2; FAILED=$((FAILED + 1)) ;;
esac
