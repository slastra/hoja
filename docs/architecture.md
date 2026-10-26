# Transfer engine

How copies, moves and their undo work, and how to test the engine on its own.

The `hoja-transfer` crate contains the transfer engine. It is a standard
Rust library with no UI dependencies. One worker thread does each job. The UI
reads progress from atomic counters.

Each job also records what it changed, in the form that reverses it, and hands
that back when it finishes. The record stays small because a directory stands
for its contents: copying into a name that held nothing records the directory
and nothing beneath it, and a move within one filesystem is a single rename
however large the tree. Per-file records appear only where a copy merged into
a directory that already existed. Undoing a transfer is itself a job, so it
gets the same progress bar, cancel button and error report as the transfer it
is taking back.

To run the engine tests:

```sh
cargo test -p hoja-transfer
```

The reflink test needs a btrfs filesystem. To prepare one:

```sh
./scripts/btrfs-loop.sh up
HOJA_TEST_BTRFS=/tmp/hoja-btrfs/mnt cargo test -p hoja-transfer -- --ignored
```
