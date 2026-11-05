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

A move across filesystems copies each file and then deletes the source. The
engine does not delete a source immediately. It waits until a sync of the
destination filesystem succeeds, at least every two seconds or every 256 MiB.
If the sync fails, the engine keeps the sources and reports each one. Thus a
failed write or a removed drive cannot remove the only copy of a file. When
you cancel a move, the engine syncs and deletes the sources of the files that
it completed.

Before a move tries to rename a file onto a name that is in use, the
destination is not touched. The engine displaces the old file only after the
new file is fully written. A move into the folder that already holds the item
does nothing. A copy into that folder makes a "(copy)" item next to the
original and does not ask.

To run the engine tests:

```sh
cargo test -p hoja-transfer
```

The reflink test needs a btrfs filesystem. To prepare one:

```sh
./scripts/btrfs-loop.sh up
HOJA_TEST_BTRFS=/tmp/hoja-btrfs/mnt cargo test -p hoja-transfer -- --ignored
```
