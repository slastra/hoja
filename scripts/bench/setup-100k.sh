#!/bin/bash
# A directory of 100,000 files for `listing.sh`.
#
# Under ~/.cache and not /tmp, which is tmpfs here: a benchmark of reading a
# directory off a RAM disk measures the RAM disk. This is the same mistake the
# parallel-copy measurement made, and the reason that entry is on the roadmap
# under "in doubt".
set -eu
DIR=${1:-$HOME/.cache/hoja-bench/100k}
rm -rf "$(dirname "$DIR")"
mkdir -p "$DIR"
cd "$DIR"
seq 1 100000 | awk '{printf "file-%06d.dat\n", $1}' | xargs -P 8 -n 2000 touch
echo "$DIR: $(ls -U | wc -l) files on $(df --output=fstype . | tail -1)"
