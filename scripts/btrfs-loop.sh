#!/bin/bash
# Creates a loopback btrfs mount for the tier1_btrfs reflink test.
# Usage:  ./scripts/btrfs-loop.sh up    (needs sudo for the mount)
#         HOJA_TEST_BTRFS=/tmp/hoja-btrfs/mnt cargo test -p hoja-transfer -- --ignored tier1_btrfs
#         ./scripts/btrfs-loop.sh down
set -euo pipefail
BASE=/tmp/hoja-btrfs
case "${1:-up}" in
  up)
    mkdir -p "$BASE/mnt"
    [ -f "$BASE/img" ] || truncate -s 1G "$BASE/img"
    command -v mkfs.btrfs >/dev/null || { echo "install btrfs-progs"; exit 1; }
    mkfs.btrfs -f "$BASE/img" >/dev/null
    sudo mount -o loop "$BASE/img" "$BASE/mnt"
    sudo chown "$USER" "$BASE/mnt"
    echo "mounted. run: HOJA_TEST_BTRFS=$BASE/mnt cargo test -p hoja-transfer -- --ignored tier1_btrfs"
    ;;
  down)
    sudo umount "$BASE/mnt" && rm -rf "$BASE"
    ;;
esac
