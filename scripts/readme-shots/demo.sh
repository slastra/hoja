#!/usr/bin/env bash
# demo.sh DIR: a home folder to photograph. Pictures of mixed shapes from
# bgen, for the grid, and a small Rust project with uncommitted changes, for
# the git colors and a preview with syntax colors.
set -euo pipefail
D=$1
G=$D/Projects/garden
# cargo install puts bgen in ~/.cargo/bin, which a plain bash may not have.
BGEN=$(command -v bgen || echo "$HOME/.cargo/bin/bgen")
[ -x "$BGEN" ] || { echo "bgen not found" >&2; exit 1; }
mkdir -p "$D/Pictures" "$G/src" "$G/assets" "$D/Documents" "$D/Downloads" "$D/Music" "$D/Videos"

i=0
while IFS='|' read -r theme pattern w h glitch name; do
    i=$((i + 1))
    args=(-t "$theme" -p "$pattern" -W "$w" -H "$h" -s "$((i * 7 + 3))" -o "$D/Pictures/$name.png")
    [ -n "$glitch" ] && args+=(-g "$glitch")
    "$BGEN" "${args[@]}" >/dev/null &
done <<'LIST'
Rosé Pine|fbm|1600|1000||rose-pine-clouds
Kanagawa Wave|waves|1000|1500||kanagawa-tide
Catppuccin Mocha|worley|1200|1200||mocha-cells
Nord|gradient|1920|1080||nord-dusk
Everforest Dark Medium|ridged|900|1350||everforest-ridge
Dracula|plasma|1600|900|rgb-split|dracula-plasma
Gruvbox Dark|hexagons|1200|1500||gruvbox-hive
Tokyo Night|domain-warp|1920|1080||tokyo-warp
Rosé Pine Moon|spiral|1200|1200|mirror|moon-spiral
Kanagawa Dragon|ripple|1500|1000||dragon-ripple
Catppuccin Macchiato|diamonds|1000|1400||macchiato-diamonds
Nord|radial|1400|1400||nord-glow
Everforest Dark Hard|noise|1800|900|scanlines|forest-static
Dracula|stripes|900|1600||dracula-stripes
Gruvbox Material Dark|fbm|1600|1100|crystallize|gruvbox-glass
Rosé Pine|worley|1000|1500|edge-glow|rose-pine-veins
Tokyo Night|waves|1920|800||tokyo-interference
Catppuccin Frappé|domain-warp|1300|1300||frappe-marble
Kanagawa Wave|fbm|1600|1000|halftone|kanagawa-print
Nord|ridged|1000|1500||nord-peaks
Rosé Pine Dawn|gradient|1500|1000||dawn-gradient
Dracula|worley|1200|900|posterize|dracula-mosaic
Everforest Dark Soft|plasma|1100|1500|wobble|forest-haze
Gruvbox Dark|ripple|1600|1000|vhs|gruvbox-tape
LIST
wait
[ "$(ls "$D/Pictures" | wc -l)" -eq 24 ] || { echo "bgen made too few pictures" >&2; exit 1; }

cat > "$G/Cargo.toml" <<'FILE'
[package]
name = "garden"
version = "0.3.0"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
FILE
cat > "$G/README.md" <<'FILE'
# garden

Plan a vegetable bed: what to sow, when, and what grows well together.
FILE
cat > "$G/src/main.rs" <<'FILE'
use garden::{Bed, Plant, Season};

/// Sow what suits the season, and leave room between neighbours that
/// compete for the same water.
fn main() {
    let mut bed = Bed::new(4, 3);
    let season = Season::current();

    for plant in Plant::all().filter(|p| p.sows_in(season)) {
        match bed.first_free_spot(&plant) {
            Some(spot) => bed.sow(spot, plant),
            None => eprintln!("no room for {}", plant.name),
        }
    }

    println!("{bed}");
}
FILE
cat > "$G/src/lib.rs" <<'FILE'
mod bed;
mod plant;
mod season;

pub use bed::Bed;
pub use plant::Plant;
pub use season::Season;
FILE
printf 'pub struct Bed;\n' > "$G/src/bed.rs"
printf 'pub struct Plant;\n' > "$G/src/plant.rs"
printf 'pub enum Season {}\n' > "$G/src/season.rs"
printf 'target/\n' > "$G/.gitignore"
python3 -I -c "
import sys
from PIL import Image
Image.open(sys.argv[1]).resize((300, 450)).save(sys.argv[2])" \
    "$D/Pictures/everforest-ridge.png" "$G/assets/cover.png"
git -C "$G" init -q
git -C "$G" add -A
git -C "$G" -c user.name=garden -c user.email=garden@example.com commit -qm init

# What git colors: a change, two new files, and an ignored build folder.
mkdir -p "$G/target/debug"
head -c 3000000 /dev/urandom > "$G/target/debug/garden"
printf '\n/// Water every bed whose soil has dried out.\npub fn water(beds: &mut [Bed]) {}\n' >> "$G/src/lib.rs"
printf 'pub struct Weather;\n' > "$G/src/weather.rs"
printf 'Tomatoes, basil, and marigolds by the fence.\n' > "$G/notes.md"

printf 'Seed order\n- carrots\n- runner beans\n' > "$D/TODO.md"
printf 'Call the allotment office about the water butt.\n' > "$D/notes.txt"
head -c 52000000 /dev/urandom > "$D/Videos/allotment-timelapse.mkv"
head -c 8400000 /dev/urandom > "$D/Music/rain-on-the-greenhouse.flac"
head -c 1200000 /dev/urandom > "$D/Downloads/planting-guide.epub"

# Ages, so the Modified column reads as a folder in use.
touch -d '2 days ago' "$G/Cargo.toml" "$G/src/main.rs"
touch -d '3 weeks ago' "$G/README.md" "$G/assets" "$G/assets/cover.png" \
    "$G/src/bed.rs" "$G/src/plant.rs" "$G/src/season.rs"
touch -d '12 minutes ago' "$G/notes.md" "$G/src/weather.rs"
touch -d '40 minutes ago' "$G/src/lib.rs" "$G/src"
touch -d '5 hours ago' "$G/target"
n=0
for f in "$D"/Pictures/*.png; do
    n=$((n + 1))
    touch -d "$((n * 37 % 300)) hours ago" "$f"
done
touch -d '1 day ago' "$D/Documents"
touch -d '6 days ago' "$D/Downloads" "$D/Downloads"/*
touch -d '2 months ago' "$D/Music" "$D/Music"/*
touch -d '1 year ago' "$D/Videos" "$D/Videos"/*
touch -d '4 hours ago' "$D/notes.txt"
touch -d '2 weeks ago' "$D/TODO.md"
