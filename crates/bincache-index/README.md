# bincache-index

The metadata plane in RAM: `StorePathHash` to a pre-rendered response blob, with lock-free
reads.

## Owns

- **The snapshot.** A frozen tier (immutable table over the full path set, rebuilt in the
  background every N publishes or M minutes) plus a small delta tier holding paths
  published since the last freeze. Lookups check delta first, then frozen. v1 ships a
  general concurrent map behind the same lookup trait; the frozen plus delta pair is the
  planned replacement, and a PTHash-class minimal perfect hash is the named upgrade for
  the frozen tier.
- **Publication.** Both tiers hang off one snapshot pointer, stored `Release` and loaded
  `Acquire`. Retired snapshots are reclaimed through epoch-based reclamation once no
  in-flight reader can observe them. Readers take no lock and write no shared cache line.
  Ingest is the only writer, on a batch cadence.
- **The arena.** An append-only region of explicit 2 MB huge pages (`MAP_HUGETLB`, THP
  off) holding the pre-rendered narinfo response blobs the index points at. Allocation
  within a publish batch is bump-style; frees are epoch-deferred alongside the snapshot
  that retired them.
- **Snapshot archival.** `rkyv` archive and zero-copy load of the frozen tier, through the
  checked API because the file on disk can be torn.
- **The filter tier, later.** A binary fuse filter over the key set that answers
  "definitely absent" from a structure small enough to stay L3-resident, so a
  miss-dominated mass query never reaches the index. Ship without it; add it when the
  index working set demonstrably exceeds L3.

## Does not own

Which bytes go in a blob (that is `bincache-core`), where snapshot files live on disk
(that is `bincache-store`), or when to publish (that is `bincache-ingest`).

## Depends on

`bincache-core`.

## Notes

One of two places licensed to carve out `#![allow(unsafe_code)]` at the module that needs
it: the arena and its mapping.

## Design references

DESIGN.md: "Index structure: frozen base + delta, published via RCU", "Hasher", "Negative
lookups: the filter tier", "Pre-rendered responses in a huge-page arena".
