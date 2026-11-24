# A folder. See picker-folder.sh.
HOJA_ARGS=--pick
APP_ID=hoja-picker
HOJA_STDIN="$OUT/request.json"
printf '{ "directory": true }\n' >"$HOJA_STDIN"
