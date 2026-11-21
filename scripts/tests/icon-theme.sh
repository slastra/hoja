# File icon themes: a Zed icon theme colours the icons, and a theme given by
# family follows the colour theme between dark and light.
#
#   FIX=$(scripts/tests/setup-grid.sh)
#   scripts/sway-harness.sh "$FIX" scripts/tests/icon-theme.sh
#
# The icon theme is one written here, so the test does not depend on what is
# installed: images red, Rust blue. It goes where hoja looks for its own,
# under the harness's private config.

wait_for 'p[0]["row_count"] == 44'
expect 'd["icon_theme"] == "Zed (Default)"' "with nothing set, the default icons"

theme="$XDG_CONFIG_HOME/hoja/icon_themes/test-icons"
mkdir -p "$theme/icon_themes" "$theme/icons"
for color in red:#e02020 blue:#2050e0; do
    cat > "$theme/icons/${color%%:*}.svg" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><rect width="16" height="16" fill="${color#*:}"/></svg>
SVG
done
cat > "$theme/icon_themes/test.json" <<'JSON'
{
  "$schema": "https://zed.dev/schema/icon_themes/v0.2.0.json",
  "name": "Test Icons",
  "author": "hoja",
  "themes": [
    { "name": "Test Icons Dark", "appearance": "dark",
      "file_icons": { "image": { "path": "./icons/red.svg" }, "rust": { "path": "./icons/blue.svg" } } },
    { "name": "Test Icons Light", "appearance": "light",
      "file_icons": { "image": { "path": "./icons/blue.svg" }, "rust": { "path": "./icons/red.svg" } } }
  ]
}
JSON

settings="$XDG_CONFIG_HOME/hoja/settings.json"
printf '{ "theme": "Rosé Pine", "icon_theme": "Test Icons" }\n' > "$settings"
wait_for 'd["icon_theme"] == "Test Icons Dark"' 20
expect 'd["icon_theme"] == "Test Icons Dark"' "a family takes the variant of a dark colour theme"

# The icon of an image row is drawn in the theme's own red: the SVG's
# colours, not the content colour every icon used to be tinted with.
sleep 0.5
shot icons-dark
IMG=$(named img-01.png)
expect "__import__('PIL.Image').Image.open('$OUT/icons-dark.png').convert('RGB').getpixel(($WX + 16, $(row "$IMG")))[0] > 180" \
    "and the icon is drawn in the theme's colours"

printf '{ "theme": "Rosé Pine Dawn", "icon_theme": "Test Icons" }\n' > "$settings"
wait_for 'd["icon_theme"] == "Test Icons Light"' 20
expect 'd["icon_theme"] == "Test Icons Light"' "and follows it to light"

printf '{ "theme": "Rosé Pine" }\n' > "$settings"
wait_for 'd["icon_theme"] == "Zed (Default)"' 20
expect 'd["icon_theme"] == "Zed (Default)"' "removing the setting puts the default back"
