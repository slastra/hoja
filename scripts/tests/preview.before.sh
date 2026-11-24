# Runs before hoja starts (see sway-harness.sh): hoja reads the thumbnailers
# once, at its first look at a file.

# A thumbnailer for MP4 that takes two seconds, so the place kept for the
# picture is on screen long enough to see. It goes in the harness's private
# XDG_DATA_HOME, which comes before /usr/share, so it wins for its types.
mkdir -p "$XDG_DATA_HOME/thumbnailers"
cat > "$OUT/slow-thumbnailer" <<'SH'
#!/bin/sh
sleep 2
exec gst-video-thumbnailer --input-uri "$1" --output "$2" --size "$3"
SH
chmod +x "$OUT/slow-thumbnailer"
printf '[Thumbnailer Entry]\nExec=%s %%u %%o %%s\nMimeType=video/mp4;\n' "$OUT/slow-thumbnailer" \
    > "$XDG_DATA_HOME/thumbnailers/slow.thumbnailer"
