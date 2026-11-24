# Previews from thumbnailers that hoja knows nothing about: 3D printing files
# from hoja-model-thumbnailer, and a font from the desktop's own thumbnailer.
#
#   cargo build --workspace
#   scripts/sway-harness.sh "$(scripts/tests/setup-model-preview.sh)" scripts/tests/model-preview.sh
#
# The model thumbnailer is the debug build in this tree, registered in the
# harness's private XDG_DATA_HOME by model-preview.before.sh, so the test does not need it installed and
# does not see an installed one. The harness points XDG_CACHE_HOME into the
# out-dir, so the pictures made here never reach the shared cache.

# model-preview.before.sh registers the thumbnailer before hoja starts.

wait_for 'p[0]["row_count"] == 7'

# One row at a time. The probe names the kind once the work is done.
look() {
    local name=$1 kind=$2 what=$3
    hover "$(named "$name")"
    wait_for "d['preview'] == '$kind: $name'" 20
    expect "d['preview'] == '$kind: $name'" "$what"
    shot "model-${name%%.*}-${name##*.}"
}

look cube.stl picture "a binary STL is drawn"
look tower.stl picture "an ASCII STL is drawn"
look plate.3mf picture "a 3MF shows the picture a slicer put in it"
look mesh.3mf picture "a 3MF with no picture is drawn from its mesh"
look part.gcode picture "G-code shows the picture a slicer put in it"
look old.gcode text "G-code with no picture shows its text"
look font.ttf picture "a font gets a picture from the desktop's thumbnailer"

# The pictures went into the cache, under the spec's names: six of them, and
# a failure mark for the old G-code.
THUMBS=$(find "$XDG_CACHE_HOME/thumbnails" -name '*.png' -path '*large*' | wc -l)
FAILS=$(find "$XDG_CACHE_HOME/thumbnails/fail" -name '*.png' 2>/dev/null | wc -l)
if [ "$THUMBS" -eq 6 ] && [ "$FAILS" -eq 1 ]; then
    echo "  ok   six pictures and one failure went into the cache"
else
    echo "  FAIL six pictures and one failure went into the cache: found $THUMBS and $FAILS" >&2
    FAILED=$((FAILED + 1))
fi

# The picture in the 3MF is 64 pixels square and is kept at its size, so a
# mesh drawn at 256 cannot pass for it. The tall tower gives a tall picture.
python3 -I - "$XDG_CACHE_HOME/thumbnails" <<'PY' && echo "  ok   the pictures are the right ones" || { echo "  FAIL the pictures are the right ones" >&2; FAILED=$((FAILED + 1)); }
import struct, sys, pathlib
def read(path):
    data = path.read_bytes()
    w, h = struct.unpack(">II", data[16:24])
    texts, at = {}, 8
    while at < len(data):
        n, kind = struct.unpack(">I4s", data[at:at + 8])
        body = data[at + 8:at + 8 + n]
        if kind == b"tEXt":
            k, v = body.split(b"\0", 1)
            texts[k.decode()] = v.decode()
        at += 12 + n
    return w, h, texts
by_name = {}
for path in pathlib.Path(sys.argv[1]).glob("large/*.png"):
    w, h, texts = read(path)
    by_name[texts["Thumb::URI"].rsplit("/", 1)[-1]] = (w, h)
w, h = by_name["plate.3mf"]
assert (w, h) == (64, 64), (w, h)
w, h = by_name["part.gcode"]
assert (w, h) == (32, 32), (w, h)
w, h = by_name["tower.stl"]
assert h > 2 * w, (w, h)
w, h = by_name["cube.stl"]
assert max(w, h) == 256, (w, h)
PY

# In the grid, each file with a picture gets it on its card.
key -M ctrl -k 2 -m ctrl
wait_for 'p[0]["layout"] == "grid"'
wait_for 'sum(c[5] for c in p[0]["cards"]) == 6' 20
expect 'sum(c[5] for c in p[0]["cards"]) == 6' "six cards show a picture"
shot model-grid
