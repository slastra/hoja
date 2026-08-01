# Pane — Transfer Engine Plan

Revision of the tiered dispatch strategy. The tier structure survives intact — it is
the right shape. The adjustments: syscall-level capabilities are *attempted*, not
classified; the correctness layer that actually beats Nautilus gets its own chapter;
local delta-patching is demoted from automatic escalation to opt-in; and the milestones
are ordered so the visible win lands first.

## Goal, stated measurably

"Moves files better than Nautilus" means, concretely:

1. **Many small files:** ≥3× gio/Nautilus throughput copying a Linux kernel tree
   cross-filesystem (Nautilus is serial with per-file overhead; this is the headline).
2. **Large files:** saturate the slower device. `cp` already does this — the bar is
   *not losing* to cp while adding progress/cancel.
3. **Same-fs copy on btrfs/XFS:** near-instant via reflink. (Caveat: gio uses
   `copy_file_range`, and btrfs implements that as a reflink internally — Nautilus may
   already be fast here. Benchmark before claiming.)
4. **No dialog storms:** errors queue and present once; conflicts batch with
   apply-to-all. Nautilus interrupts per file.
5. **Honest completion on removable media:** "done" means data is on the device, not
   in the page cache. Nautilus reports done, then eject stalls for a minute. We syncfs
   before reporting completion on removable destinations, with device-speed progress.
6. **Pause, cancel, resume** on every operation, including across app restarts.

Items 4–6 are UX correctness, not throughput — and they are the differences a user
retells. The parallel engine is necessary but not sufficient.

## Core principles

- **Attempt-and-fallback beats classify-and-dispatch** for anything the kernel decides:
  `rename()`, `FICLONE`, `copy_file_range`. Try the cheap syscall, catch
  `EXDEV`/`EOPNOTSUPP`, fall through. Classification logic that predicts kernel
  behavior (st_dev comparison, fs-type sniffing) is where this code rots — btrfs
  subvolumes share a filesystem but differ in st_dev; bind mounts share st_dev but
  fail `rename()`.
- **Cache the fallback, not the prediction.** After the first `EXDEV` for a
  (src-mount, dst-mount) pair — mount IDs via `statx` `STATX_MNT_ID` — skip the
  attempt for the rest of the job. First file probes, rest of the job knows.
- **One walk.** Dispatch never blocks on a full pre-scan. The gate counters (size,
  file count) accumulate during the same walk that feeds the copy queue; if totals
  cross the Tier 2 gates mid-operation, the job escalates to Tier 3 in place.
- **Per-file escalation, never whole-operation.** One conflicted file must not drag
  the other N onto a slower path.
- **Every tier reports through one progress/completion interface.** The UI is
  tier-agnostic. Progress is an atomic byte counter sampled on a 100–200ms timer,
  never event-per-file.

## Tiers

```
move  + rename() succeeds              → Tier 0  (atomic rename)
copy  + FICLONE succeeds               → Tier 1  (reflink clone)
copy  + below gates                    → Tier 2  (plain copy)
copy  + above gates                    → Tier 3  (parallel bulk)
sync  (explicit)                       → Tier 4  (chunked dedup)
remote pane↔pane                       → Tier 5  (Tier 4 + QUIC)
```

### Tier 0 — move via rename

Try `rename()` first for every move. `EXDEV` → copy-then-delete through the
appropriate copy tier, where **delete happens only after the copy of that file is
complete and verified** — a failed cross-fs move must never lose the source.
Note `rename()` fails across bind mounts of the same filesystem; this is exactly why
we attempt rather than compare st_dev.

### Tier 1 — reflink clone

`FICLONE` via `reflink-copy`, attempted per job (then cached per mount pair as above).
A 50GB clone costs the same as 1KB. Two things the draft omitted:

- Reflink clones *data*. Metadata (times, mode, xattrs) still needs the same
  preservation pass as every other tier.
- Same-fs copy where reflink is unsupported (ext4) falls through to Tier 2/3 — whose
  `copy_file_range` stays in-kernel on the same fs and is already near-optimal. The
  draft's Tier 2 wording ("cross-filesystem copies") accidentally excluded this case;
  Tier 2 applies to *any* copy below the gates.

### Tier 2 — plain copy

Gates (configurable, set by benchmark not guess): total < 100MB **and** count < 1000.

- `copy_file_range` with a fallback loop over one reusable 1–4MB buffer. Two caveats
  from the man page, both load-bearing: cross-fs `copy_file_range` works only on
  ≥5.19 *and* same fs type; and on 5.3–5.18 some virtual filesystems **reported
  success while copying nothing**. Always compare resulting size; keep the fallback
  path alive permanently.
- Sparse files handled here too, not just in bulk: `SEEK_DATA`/`SEEK_HOLE` loop,
  copying data extents only. A sparse VM image must not inflate.
- No chunking, no hashing, no store. Nothing to delta against.

### Tier 3 — parallel bulk

- Walker (`jwalk`) feeds a work-stealing queue; file-level parallelism for small
  files, a separate sequential streaming path for individual large files.
- **Thread pool with plain syscalls first; io_uring second, behind a benchmark.**
  Parallelism is ~90% of the win over Nautilus. io_uring's genuine edge is batching
  open/stat/read across thousands of small files — add it when the benchmark shows
  the syscall path leaving throughput on the table, not before. If added, use the
  raw `io-uring` crate on dedicated worker threads — `monoio`/`tokio-uring` are
  whole-crate runtime commitments and GPUI has its own executors.
- Concurrency adapts to the destination: `/sys/block/*/queue/rotational` → low
  parallelism on spinning disks (seek thrash), scaled up on NVMe.
- Preflight `statvfs` on the destination: fail an obviously-won't-fit job before
  writing 200GB of a 300GB copy.

### Tier 4 — chunked dedup (explicit sync only)

FastCDC → BLAKE3 → store diff → transfer missing chunks → verify. Three demotions
from the draft:

- **Not triggered by "destination already holds a previous version."** In a file
  manager, dest-exists is a *conflict* — surface it (overwrite / skip / keep both /
  apply-to-all) before any machinery runs. Tier 4 runs when the user asked for sync.
- **Local delta-patch rarely pays and is deferred.** Diffing requires reading the
  entire dest *and* the entire source, then writing deltas; plain overwrite reads
  source and writes dest once. Delta wins only when the file is mostly unchanged and
  the medium is slow — which is the remote case. Worse, atomic-replace (temp+rename)
  forfeits the delta advantage entirely unless the temp starts as a reflink of the
  dest, and in-place patching trades away crash safety. Opt-in later, benchmark-gated.
- The chunk store earns its complexity for **remote transfer and repeat-backup
  dedup**. It is not a local staging area — local copies never write through it
  (that would double both reads and writes).

### Tier 5 — remote (pane↔pane)

Tier 4 + `quinn`, zstd on chunks (**adaptive** — skip incompressible data by
sampling; compressing video burns CPU to slow the transfer down), chunk-hash
request/response, multiplexed streams.

Honest scope note: this requires pane on both ends. It is the Syncthing model, and a
real differentiator there — but it is not how a file manager reaches an arbitrary
SFTP/SMB/NFS box. Operating *well on mounted network filesystems* (detect network fs,
tune concurrency, minimize per-file round trips) is a separate, earlier concern, and
QUIC's raw single-stream throughput can trail well-tuned TCP — the wins are
multiplexing many small objects, 0-RTT resume, and connection migration.

## The correctness layer (cross-cutting, every tier)

This chapter is most of "better than Nautilus," and none of it was in the draft:

- **Metadata:** mode, mtime (`utimensat`), xattrs (`copy_xattr`), ownership where
  permitted. Fail-soft with a warning when the destination fs can't hold them (FAT).
- **Symlinks copied as links**, never followed, matching every file manager.
- **Hardlinks preserved** within a job via an (st_dev, st_ino) → first-dest map, or a
  tree with hardlinks silently multiplies in size.
- **Atomicity:** write to `.pane-partial-<name>`, fsync, rename over. Crash leaves
  either the old file or the new one, never a torn one.
- **Conflict policy** as an engine-level enum surfaced to the UI once with
  apply-to-all, not per file.
- **Error queue:** failures collect and report at completion (with per-file retry);
  the job continues. Abort-on-first-error is a policy option, not the default.
- **Job journal:** every job persists its plan and per-file state. This one
  mechanism buys pause/resume, resume-after-crash, resume-after-app-restart, *and*
  the undo record (inverse operations). Design it early; it shapes the API.
- **Durability:** default matches `cp` (no fsync-per-file — it craters small-file
  throughput), but removable destinations get `syncfs` before completion is reported,
  with progress attributed to the flush.
- **Trash:** delete = freedesktop trash spec, with real deletion as the explicit
  variant.

## Opportunistic index population

Kept as drafted (background chunk-indexing of destinations after bulk copies, so a
later sync has a baseline), with two corrections: idle ioprio is only honored by BFQ,
so the indexer self-throttles and yields to any foreground job rather than trusting
the scheduler; and indexed destinations get a watcher (inotify — fanotify's
whole-filesystem mode needs CAP_SYS_ADMIN and is unavailable to a desktop app) so the
index invalidates instead of silently staling.

## Engine ↔ UI boundary

`pane-transfer` is a standalone crate: std threads + channels, zero GPUI types. The
UI side holds a `Job` handle exposing atomic progress counters, a control channel
(pause/cancel), and an event receiver (conflicts, errors, completion). GPUI polls
progress on its own timer. The earlier context-menu work consumes exactly this
handle: cut/copy/paste is clipboard state + one engine call.

## Milestones

| | Deliverable | Beats Nautilus at |
|---|---|---|
| M1 | Dispatch + Tiers 0/1/2 + correctness layer + cancel + progress | reflink instant-copy; no dialog storms; honest removable completion |
| M2 | Context menu (cut/copy/paste, open, open-with) on the M1 engine | — (UX parity gate) |
| M3 | Tier 3 thread-pool bulk + conflict batching + error queue | the kernel-tree benchmark |
| M4 | Job journal: pause/resume/undo/trash | operation robustness |
| M5 | io_uring under Tier 3 **if** benchmarks justify | small-file ceiling |
| M6 | Tier 4 store + explicit sync (property-tested round-trip) | repeat sync near-instant |
| M7 | Tier 5 pane↔pane QUIC | remote delta sync |

## Testing

- **Dispatch table-test** over (op kind × mount relation × reflink × size class ×
  gates × locality) asserting the chosen tier — as drafted, kept.
- **Property test:** Tiers 1/2/3 (and later 4) produce byte-identical output from the
  same source; chunk→diff→reassemble round-trips byte-identical.
- **Fixture tree** containing: hardlink pairs, dangling and cyclic symlinks, sparse
  files, xattrs, a >255-byte name, a file with no read permission, a FIFO. Every
  copy tier must handle all of it, with the permission failure landing in the error
  queue, not aborting.
- **Crash test:** `kill -9` mid-job; on restart the journal resumes and the
  destination contains no torn files.
- **ENOSPC test:** preflight rejection, and graceful mid-job handling when another
  writer fills the disk anyway.
- **Benchmarks** (criterion, in CI): vs `cp`, `rsync`, and `gio copy` (Nautilus's
  actual engine — benchmarkable headlessly) on: kernel tree, single 10GB file, 10GB
  file with a 1MB edit (Tier 4's showcase), mostly-unchanged repeat sync. On NVMe
  and spinning disk. Gate thresholds get set from these numbers.

## Crates

| Purpose | Crate |
|---|---|
| Reflink | `reflink-copy` |
| Syscalls (statx, copy_file_range, utimensat, xattrs) | `rustix` |
| Walker | `jwalk` (not `ignore` — gitignored bytes are still bytes) |
| Work queue | `crossbeam-deque` (or rayon) |
| Chunking / hashing | `fastcdc`, `blake3` |
| Store index / job journal | `redb` |
| io_uring (M5) | `io-uring` (raw; no runtime commitment) |
| Remote | `quinn`, `zstd` |
| Watching | `notify` (inotify backend) |
| Benchmarks | `criterion` |
