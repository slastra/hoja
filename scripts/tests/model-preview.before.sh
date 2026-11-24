# Runs before hoja starts (see sway-harness.sh): hoja reads the thumbnailers
# once, at its first look at a file.
#
# The model thumbnailer is the debug build in this tree, not an installed one.
# The private XDG_DATA_HOME comes before /usr/share, so it wins for its types.

THUMBNAILER="$PWD/target/debug/hoja-model-thumbnailer"
[ -x "$THUMBNAILER" ] || { echo "no $THUMBNAILER: run cargo build --workspace" >&2; exit 1; }
mkdir -p "$XDG_DATA_HOME/thumbnailers"
sed -e "s|^TryExec=.*|TryExec=$THUMBNAILER|" -e "s|^Exec=hoja-model-thumbnailer|Exec=$THUMBNAILER|" \
    packaging/hoja-model.thumbnailer > "$XDG_DATA_HOME/thumbnailers/hoja-model.thumbnailer"
