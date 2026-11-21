# Hover previews: the pointer rests on a row and a tooltip shows the file.
#
#   FIX=$(scripts/tests/setup-preview.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/preview.sh
#
# Needs ffmpeg for the fixture and the system's thumbnailers for the pictures.
# The harness points XDG_CACHE_HOME into the out-dir, so the thumbnails made
# here never reach the shared cache.

wait_for 'p[0]["row_count"] == 6'

# A thumbnailer for MP4 that takes two seconds, so the place kept for the
# picture is on screen long enough to see. It goes in the harness's private
# XDG_DATA_HOME, which comes before /usr/share, so it wins for its types. Hoja
# reads the thumbnailers at its first picture, which is after this.
mkdir -p "$XDG_DATA_HOME/thumbnailers"
cat > "$OUT/slow-thumbnailer" <<'SH'
#!/bin/sh
sleep 2
exec gst-video-thumbnailer --input-uri "$1" --output "$2" --size "$3"
SH
chmod +x "$OUT/slow-thumbnailer"
printf '[Thumbnailer Entry]\nExec=%s %%u %%o %%s\nMimeType=video/mp4;\n' "$OUT/slow-thumbnailer" \
    > "$XDG_DATA_HOME/thumbnailers/slow.thumbnailer"

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

# While the picture is made, the facts are shown and a place of the right
# shape is kept for it. The shots show the two are the same shape.
hover "$(named portrait.mp4)"
wait_for "d['preview'] == 'loading video: portrait.mp4'" 5
expect "d['preview'] == 'loading video: portrait.mp4'" "the facts show before the picture"
shot preview-portrait-loading
wait_for "d['preview'] == 'video: portrait.mp4'" 20
expect "d['preview'] == 'video: portrait.mp4'" "then the picture takes the place kept for it"
shot preview-portrait

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
