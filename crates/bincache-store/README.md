# bincache-store

The filesystem as the payload database. Artifacts are immutable and content-addressed, so
recovery never has to answer "which version", only "present or absent".

## Owns

- **On-disk layout.** `nar/<shard>/<file hash>.nar.zst`, where the shard is the leading
  characters of the name. The served URL stays flat; the split is private to the
  filesystem.
- **Atomic appearance.** An upload streams into a staging file, is `fsync`ed, then renamed
  into its content-addressed name. It appears whole or not at all. A crash leaves a staging
  file rather than a half-artifact, and the boot sweep collects those.
- **Read-side access.** An open handle plus the size a `Content-Length` needs. The
  descriptor keeps the file alive across an unlink, so a delete during a stream finishes
  the stream safely and POSIX does the reference counting.
- **Reconciliation input.** The scan that lists what is actually on disk, separating names
  that parse from names that do not, since an unparseable name means something other than
  bincache wrote there.

## Does not own

Any in-RAM index state, and any policy about what to evict. It performs the unlink; it does
not decide it.

## Depends on

`bincache-core`.

## Notes

Placement is staging plus `rename` rather than `O_TMPFILE` plus `linkat`. Both give atomic
appearance; `linkat(AT_EMPTY_PATH)` needs privilege on many kernels and is Linux-only,
which would make the durability primitive untestable elsewhere.

## Design references

DESIGN_V2.md: "The payload plane", "Durability and recovery", "Retention".
