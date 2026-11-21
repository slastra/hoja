#!/usr/bin/env bash
# Build the fixture the git test runs against.
#
#   scripts/tests/setup-git.sh [dir]
#
# A repository with one of each thing a name can be coloured for: a change
# deep in a folder, a new file, an ignored folder, and a tracked folder that
# holds an ignored file and nothing else changed. Prints the repository.
set -euo pipefail

out="${1:-${CLAUDE_JOB_DIR:-/tmp}/hoja-git}"
rm -rf "$out"
mkdir -p "$out/src" "$out/clean"
g() { git -C "$out" -c user.name=t -c user.email=t@t "$@"; }
g init -q
printf '*.log\ntarget/\n' > "$out/.gitignore"
printf 'fn main() {}\n' > "$out/src/main.rs"
printf 'a\n' > "$out/clean/a.txt"
printf 'kept\n' > "$out/kept.txt"
g add .
g commit -q -m first
printf 'x\n' > "$out/clean/debug.log"
printf 'fn main() { }\n' > "$out/src/main.rs"
printf 'n\n' > "$out/new.txt"
mkdir -p "$out/target"
printf 'o\n' > "$out/target/out"
echo "$out"
