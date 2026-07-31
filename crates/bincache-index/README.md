# bincache-index

The metadata plane's ground truth: an embedded key-value store holding narinfo records.

## Owns

- **The schema.** Two `redb` tables with `rkyv`-encoded values. `narinfo` maps a
  `storepath::Hash` to the signed record. `nar` maps a `NarHash` to the artifact that holds
  those bytes, which is what answers the `HEAD` a client sends before it decides to upload.
- **Publication.** A commit is the publish. There is no snapshot to validate at boot, no
  log to replay, and no O(n) filesystem rescan as the corruption floor, because the store
  owns crash consistency.
- **Reads.** Lookup, iteration for reconciliation and key rotation, and a count.

## Does not own

Which bytes go in a record (that is `bincache-core`), where artifacts live on disk (that is
`bincache-store`), or when to publish (that is `bincache-ingest`).

## Depends on

`bincache-core`.

## Notes

`rkyv` runs with `unaligned` and `little_endian`. `redb` hands back a byte slice at
whatever alignment its page put it, so validating an archive in place needs alignment 1 to
avoid a copy on every read, and a fixed endianness keeps the on-disk format from changing
meaning on a different target.

The RAM projection the design describes is deliberately absent. It would be a derived,
non-authoritative cache, and there is nothing profiled yet to justify one. `fjall` is the
named alternative if write throughput ever dominates, since an LSM absorbs bursts better
than a B-tree; that would be a measured swap behind this API.

## Design references

DESIGN_V2.md: "The metadata plane", "Ground truth: an embedded KV store", "RAM tier: a
projection, added later, with a ceiling".
