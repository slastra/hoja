# Several files, no filter. See picker-multiple.sh.
HOJA_ARGS=--pick
APP_ID=hoja-picker
HOJA_STDIN="$OUT/request.json"
printf '{ "multiple": true }\n' >"$HOJA_STDIN"
