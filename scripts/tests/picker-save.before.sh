# A save of one file, with a name and filters. See picker-save.sh.
HOJA_ARGS=--pick
APP_ID=hoja-picker
HOJA_STDIN="$OUT/request.json"
cat >"$HOJA_STDIN" <<'JSON'
{ "save": true, "current_name": "report.txt",
  "filters": [
    { "name": "Text", "globs": ["*.txt"] },
    { "name": "All", "globs": ["*"] }
  ] }
JSON
