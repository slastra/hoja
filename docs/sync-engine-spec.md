# Pane — File Sync Engine Implementation Spec

> **Historical document.** The tiered strategy in
> [`transfer-plan.md`](transfer-plan.md) supersedes this spec. This file stays
> as a record of the initial design.

## Project Context

Pane is a file manager built on GPUI (Zed's Rust GPU-accelerated UI framework). This document specifies the core file sync/copy engine — the primary differentiator of the project. The goal is a state-of-the-art local and remote file transfer engine that surpasses rsync's 1996-era design, implemented as a standalone Rust crate that the GPUI frontend consumes.

## Phase 1 — Fast-path short-circuit (build first, gives immediate wins)

Before any chunking/hashing, always attempt the cheapest possible path:

1. Detect if source and destination share a filesystem.
2. If same filesystem and it supports reflink (btrfs, XFS w/ reflink, some ZFS configs): use `FICLONE` via `copy_file_range`/`reflink_copy` crate. Near-instant, zero data duplication.
3. If same filesystem but no reflink support: fall back to plain `copy_file_range` (still avoids userspace buffer round-trips).
4. Only fall through to the full chunking pipeline below when crossing filesystems/devices, or for actual bidirectional sync (not one-shot copy).

Crates: `reflink-copy`, `rustix`.

Deliverable: a `FastPath` module with a `try_fast_copy(src, dst) -> Result<bool>` that returns whether it succeeded, so callers fall through automatically.

## Phase 1b — Directory size scanning (dust-style)

Folder sizes in the file listing should be computed the way `dust` (bootandy/dust, "du + rust") does it, not with a naive recursive walk:

1. **Parallel tree walk.** Use `rayon` to fan the walk out across threads rather than a single-threaded recursive descent — this is dust's core speed advantage over `du`. Pair with `ignore` (the crate from ripgrep) for the walker itself if you want gitignore-aware traversal for free, or plain `jwalk` if you don't need ignore-file awareness.

2. **Smart recursion, not full-depth-always.** Dust doesn't blindly recurse to the bottom of every directory — it recurses further into subdirectories that are contributing meaningfully to the total and stops early on small ones once a reasonably accurate top-N picture is available. For Pane, this matters most for the *initial* size shown in a listing: get an approximate number fast, refine in the background, and update the displayed value as refinement completes rather than blocking the UI on a full walk.

3. **Apparent size vs. actual (block) size — pick one and label it.** `du`/dust distinguish between apparent size (sum of file sizes) and actual disk usage (sum of allocated blocks, which differs due to filesystem block size and sparse files). Decide which Pane shows by default (apparent size matches user intuition better for "how big is this folder"; block size matches "how much disk will I free") and expose the other as a toggle, same as dust's `--apparent-size` flag.

4. **Symlinks not followed by default.** Match dust's default here — following symlinks during a size walk can double-count or infinite-loop on cyclical links. Make it an opt-in flag if you support it at all.

5. **Handle permission errors gracefully.** Don't fail the whole scan on one unreadable directory — dust prints a single consolidated "did not have permission" notice rather than one error per denied path. Surface partial results with a visual indicator that the total may be incomplete, rather than blocking or silently showing a wrong number.

6. **Incremental updates via the Phase 7 watcher, not re-scans.** Once the initial parallel walk completes and populates the chunk-store-backed size index (see the sync-engine note below), subsequent size changes should come from filesystem watch events updating the cached aggregate — not from re-running the walker. The dust-style parallel walker is for the *cold-start* case (first time a directory is opened, or index invalidation); it shouldn't be the steady-state mechanism.

7. **Cache and reuse across the chunk store.** Because Pane's sync engine already computes per-file hashes and sizes during chunking (Phase 2), the size index should share that data rather than maintaining size as a separate computation — a file that's already been chunked has its size known for free.

Crates: `rayon`, `ignore` (or `jwalk`), plus whatever your existing `pane-store` index uses for the aggregate cache.

Deliverable: a `DirSizeScanner` with `scan(path) -> Stream<SizeUpdate>` — emits progressively refined totals rather than a single blocking return, so the GPUI list view can show "calculating…" then fill in and refine, matching dust's fast-first-impression behavior rather than a spinner-then-final-number model.

## Phase 2 — Content-defined chunking + hashing

1. Implement chunking via `fastcdc` crate (FastCDC algorithm) rather than fixed block sizes. Target average chunk size ~64KB–1MB depending on typical file sizes in testing — make this configurable.
2. Hash every chunk with BLAKE3 (`blake3` crate). Use BLAKE3's native tree structure — do not bolt on a separate Merkle tree implementation, it already provides incremental/streaming verification.
3. Build a `ChunkedFile` representation: ordered list of `(offset, length, blake3_hash)` per file.
4. Whole-file hash = BLAKE3 hash of the file (cheap to compute in parallel with chunking via BLAKE3's SIMD parallelism) — used for fast "is this file identical" short-circuit before even comparing chunk lists.

Deliverable: `chunk_file(path) -> ChunkedFile`, fully unit-testable independent of any transfer logic.

## Phase 3 — Content-addressable chunk store

1. On-disk layout: `<store_root>/chunks/<first-2-hex-chars-of-hash>/<full-hash>`, raw chunk bytes, no metadata (metadata lives separately).
2. Index: a simple embedded DB (recommend `redb` — pure Rust, no C deps, good for this access pattern) mapping `file_path -> ChunkedFile` and tracking chunk reference counts for garbage collection.
3. Dedup logic: before writing any chunk to the store, check if its hash already exists — if so, skip the write and just increment refcount / add the reference. This is what gives cross-file and cross-sync dedup for free.
4. Implement a GC pass that removes orphaned chunks (refcount zero) — run on demand, not automatically, to avoid surprising deletions mid-operation.

Deliverable: `ChunkStore` with `put_chunk`, `get_chunk`, `has_chunk`, `gc()`.

## Phase 4 — Diff engine

1. Given a `ChunkedFile` for source and the stored `ChunkedFile` for the same path at destination (if any), compute the set of chunk hashes present at dest vs. needed from source.
2. This is a straightforward hash-set diff — the CDC step already did the hard work of aligning chunks correctly across edits.
3. Output a `TransferPlan`: list of chunks to send, list of chunks already available locally (for reassembly instructions), in what order to reassemble.

Deliverable: `diff(source: ChunkedFile, dest: Option<ChunkedFile>) -> TransferPlan`.

## Phase 5 — Local reassembly via io_uring

1. For local-to-local transfers (different filesystems, no reflink available), use io_uring for the actual write-out rather than blocking read/write loops.
2. Recommend `rustix` for direct io_uring syscall access if you want full control, or `monoio`/`tokio-uring` if you want an async runtime that's io_uring-native throughout (monoio is a stronger fit for a from-scratch design; tokio-uring if you want tokio ecosystem compatibility).
3. Batch: queue reads for all needed chunks and writes for reassembly without waiting on each individually; let the kernel overlap them.
4. Verify each reassembled file against its whole-file BLAKE3 hash before considering the copy complete.

Deliverable: `reassemble(plan: TransferPlan, dest_path) -> Result<()>` using io_uring under the hood, falling back to standard blocking I/O on non-Linux (this is Linux-only optimization — plan for a portable fallback path from day one so macOS/other targets aren't dead ends).

## Phase 6 — Remote transport (QUIC)

Only needed once local sync is solid — sequence this after Phases 1–5 are working and benchmarked.

1. Use `quinn` for QUIC. Multiplex chunk transfers across streams so one large/slow chunk doesn't head-of-line-block others — this is the concrete advantage over rsync's single TCP stream.
2. Compress chunks in-flight with zstd (`zstd` crate, use a mid-range compression level — benchmark the tradeoff, don't default to max compression).
3. Protocol: request/response per chunk hash (client sends list of needed hashes, server streams back chunks it has), not a single serialized diff blob — this lets you pipeline requests and start writing before the full plan is transferred.
4. Authentication/transport security: QUIC gives you TLS 1.3 by default via `quinn` — use it, don't roll a custom auth layer initially. Start with a shared-key or SSH-agent-based handshake for the MVP.

Deliverable: `pane-transport` crate exposing a `SyncServer` and `SyncClient` that speak chunk-hash request/response over QUIC.

## Phase 7 — Live change detection (for actual "sync", not one-shot copy)

1. Use `notify` crate as the cross-platform abstraction, but on Linux prefer fanotify over inotify for whole-tree watching — fanotify doesn't require a watch descriptor per directory, which matters at scale.
2. On change event: re-chunk only the changed file (not the whole tree), diff against the stored `ChunkedFile`, and propagate the delta.
3. Debounce rapid successive writes to the same file (editors often do multiple writes per save) before triggering a sync pass.

Deliverable: `Watcher` that emits `FileChanged(path)` events feeding back into Phase 4's diff engine.

## Phase 8 — Conflict handling (only if bidirectional sync is in scope)

1. Track a simple version vector or last-modified-plus-origin-id per file, not full CRDTs — this is a file manager, not a structured-data sync tool.
2. On genuine conflict (both sides changed since last common state): keep both, suffix the losing side (`file.txt` → `file.sync-conflict-<timestamp>-<origin>.txt`), same pattern as Syncthing. Never silently discard data.

Deliverable: `ConflictResolver` invoked by the diff engine when both sides have diverged from the last known-common `ChunkedFile`.

## Testing/Benchmarking priorities

- Correctness first: property-test the chunk/diff/reassemble round-trip (chunk a file, diff against empty, reassemble, verify byte-identical) before optimizing anything.
- Benchmark against rsync and plain `cp`/`tar` on: (a) single large file, (b) many small files, (c) large file with small localized edit (this is where CDC should visibly beat rsync's fixed blocking), (d) repeated sync of mostly-unchanged tree (dedup should make this near-instant).
- Use `criterion` for benchmarks, commit them to CI so regressions are caught.

## Suggested build order (summary)

1. Fast-path reflink short-circuit (Phase 1) — smallest scope, immediate real-world win, good first milestone.
2. Directory size scanning (Phase 1b) — independent of the chunking pipeline, can be built and shipped in the UI early for immediate user-visible value.
3. Chunking + hashing (Phase 2) — foundational, everything else depends on it.
4. Chunk store (Phase 3) — needed before diffing makes sense.
5. Diff engine (Phase 4) — where CDC's advantage over rsync becomes measurable.
6. io_uring reassembly (Phase 5) — local sync engine now end-to-end functional. Good point for first public benchmark comparison against rsync/cp.
7. QUIC transport (Phase 6) — only once local path is proven.
8. Live watching (Phase 7) and conflict handling (Phase 8) — needed for continuous sync, not needed for one-shot copy use case. Phase 7's watcher also becomes the steady-state mechanism for keeping Phase 1b's size index fresh.

## Crate reference list

| Purpose | Crate |
|---|---|
| Directory size scanning | `rayon`, `ignore` (or `jwalk`) |
| Content-defined chunking | `fastcdc` |
| Hashing | `blake3` |
| Chunk store index | `redb` |
| Reflink/CoW copy | `reflink-copy` |
| Low-level syscalls, io_uring | `rustix` |
| io_uring async runtime | `monoio` or `tokio-uring` |
| QUIC transport | `quinn` |
| Compression | `zstd` |
| Filesystem watching | `notify` |
| Directory scanning | `jwalk` |
| Benchmarking | `criterion` |
