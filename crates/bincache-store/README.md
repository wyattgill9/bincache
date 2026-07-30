# bincache-store

The filesystem as ground truth. Everything in RAM is a cache of what this crate owns.

## Owns

- **On-disk layout and naming.** Content-addressed NAR files at `nar/<filehash>.nar.zst`
  under a flat sharded directory tree, the per-path narinfo metadata records that response
  blobs are regenerated from during a signing-key rotation, the append-only publish log,
  and the `rkyv` index snapshot files.
- **Durability primitives.** Stream into `O_TMPFILE`, `fsync`, then `linkat` to the
  content-addressed name, so a path appears atomically or not at all. A crash leaves
  either nothing or an orphan file, which makes a client retry a no-op.
- **Recovery inputs.** Log append and log-tail replay, plus the O(n) filesystem rescan
  that boot falls back to when a snapshot or log fails validation.
- **Read-side file access.** The `statx`-enriched open whose descriptor `bincache-serve`
  streams from. An unlinked file finishes streaming safely because the fd holds it.
- **Cold maintenance scans.** GC and integrity re-verification, which must use
  `POSIX_FADV_DONTNEED` or `O_DIRECT` so they do not evict the page cache that serves
  traffic.

## Does not own

Any in-RAM index state, and any policy about what to evict. It performs the unlink; it
does not decide it.

## Depends on

`bincache-core`.

## Notes

The syscall surface beyond sockets lives here, through `rustix` on the linux_raw backend.

## Design references

DESIGN.md: "The Payload Plane", "Durability and Recovery", "Pipeline as a typestate
machine".
