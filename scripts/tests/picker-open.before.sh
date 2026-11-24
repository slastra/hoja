# One file, with a filter for pictures. See picker-open.sh.
HOJA_ARGS=--pick
APP_ID=hoja-picker
HOJA_STDIN="$OUT/request.json"
cat >"$HOJA_STDIN" <<'JSON'
{ "title": "Choose a picture", "accept_label": "_Attach",
  "filters": [
    { "name": "Text", "globs": ["*.txt"] },
    { "name": "Images", "globs": ["*.png"], "mimes": ["image/*"] }
  ],
  "current_filter": 1 }
JSON
