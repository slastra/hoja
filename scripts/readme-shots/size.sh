# size W H: resize the nested sway's window on the host, which is what sizes
# its output, then give hoja a moment to lay out again. Sourced by the shots.
size() {
    local addr
    addr=$(WAYLAND_DISPLAY= hyprctl clients -j |
        jq -r '.[] | select(.class == "wlroots") | .address' | head -1)
    WAYLAND_DISPLAY= hyprctl dispatch \
        "hl.dsp.window.resize({ x = $1, y = $2, window = \"address:$addr\" })" >/dev/null
    sleep 1.5
}
