# Roadmap

What is planned, and what is now in doubt.

Planned and not complete:

- **Writing to an archive.** Everything here reads; rename, delete and paste
  inside one are refusals rather than half-features.
- **Tabs, previews and thumbnails.**
- **Query history in the pickers**, and modal geometry that persists.
- **Explicit sync between two directories**, chunked so that repeating it moves
  only what changed.
- **Sync between two machines**, Hoja to Hoja.

Planned once, and now in doubt:

- **Parallel copy for many small files.** Measured against `xcp`: 4.7x faster on
  tmpfs, and *slower* than a single thread on real devices. The win was the RAM
  disk rather than the parallelism, so this needs a benchmark that is not a
  tmpfs before it is worth building. What the same measurements did point at is
  the atomic temp-file rename, which was half the time of an exFAT copy.
- **io_uring underneath it**, which was always conditional on that benchmark.
