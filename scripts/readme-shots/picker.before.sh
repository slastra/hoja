# A save dialog, as Firefox would ask for one, over the pictures as a grid.
# See picker.sh.
HOJA_ARGS=--pick
APP_ID=hoja-picker
HOJA_STDIN="$OUT/request.json"
printf '{ "theme": "Rosé Pine", "view": { "layout": "grid" } }\n' \
    >"$XDG_CONFIG_HOME/hoja/settings.json"
cat >"$HOJA_STDIN" <<'JSON'
{ "save": true, "title": "Save Image", "current_name": "garden-cover.png",
  "filters": [
    { "name": "PNG Image", "globs": ["*.png"] },
    { "name": "All Files", "globs": ["*"] }
  ] }
JSON
