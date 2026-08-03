#!/usr/bin/env bash
# Build the fixture directory the archive test runs against.
#
#   scripts/tests/setup-archives.sh [dir]
#
# Three zip files, each standing for something the survey found in the 127 real
# ones on a working machine:
#
#   fonts.zip      ordinary, with directory entries
#   bare.zip       no directory entries at all, which 3 of 12 sampled had
#   broken.zip     not a zip, which 5 of 127 were: truncated downloads and one
#                  16-byte text placeholder
#
# Written with python's zipfile rather than the `zip` binary, so the fixture
# needs nothing installed that a machine running the tests does not already have.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-archives}"
rm -rf "$out"
mkdir -p "$out"

python3 - "$out" <<'PY'
import sys, zipfile
from pathlib import Path

out = Path(sys.argv[1])

# Ordinary: directory entries present, nested a level down.
with zipfile.ZipFile(out / "fonts.zip", "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("ttf/", "")
    z.writestr("ttf/sub/", "")
    z.writestr("ttf/Inter.ttf", b"i" * 4096)
    z.writestr("ttf/sub/Mono.ttf", b"m" * 2048)
    z.writestr("LICENSE", b"l" * 128)

# The same shape with every directory entry left out, so the two listings have
# to come out identical.
with zipfile.ZipFile(out / "bare.zip", "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("ttf/Inter.ttf", b"i" * 4096)
    z.writestr("ttf/sub/Mono.ttf", b"m" * 2048)
    z.writestr("LICENSE", b"l" * 128)

(out / "broken.zip").write_text("placeholder zip\n")

# One plain file, so the fixture directory is not all archives.
(out / "notes.txt").write_text("just a file\n")
PY

# The same tree again as a tarball, once per codec, so a test can check that
# all five list identically. Built with the system tools rather than from Rust:
# the point of a fixture is to be what other programs actually produce.
work="$out/.build"
mkdir -p "$work/ttf/sub"
printf 'inter' > "$work/ttf/Inter.ttf"
printf 'mono'  > "$work/ttf/sub/Mono.ttf"
printf 'hello' > "$work/LICENSE"
# One symlink, because every toolchain tarball has them and a copy that turned
# one into a small text file holding a path would look fine until it did not.
ln -s Inter.ttf "$work/ttf/current.ttf"

tar -C "$work" --owner=0 --group=0 --numeric-owner \
    --mtime='@1700000000' --sort=name -cf "$out/fonts.tar" ttf LICENSE

# Each codec is skipped rather than failed when its tool is missing, so this
# still builds something useful on a machine without all four.
gzip  -kfn "$out/fonts.tar" 2>/dev/null || echo "no gzip, skipping .tar.gz"   >&2
bzip2 -kf  "$out/fonts.tar" 2>/dev/null || echo "no bzip2, skipping .tar.bz2" >&2
xz    -kf  "$out/fonts.tar" 2>/dev/null || echo "no xz, skipping .tar.xz"     >&2
zstd  -qf  "$out/fonts.tar" -o "$out/fonts.tar.zst" 2>/dev/null \
    || echo "no zstd, skipping .tar.zst" >&2

rm -rf "$work"

echo "$out"
