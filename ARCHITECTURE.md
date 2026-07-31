# bincache: how the parts fit

Orientation doc. Read this to reload the whole system into your head, then go to
`research/DESIGN.md` for the reasoning behind any single decision.

## Current state

Nothing is implemented. All five `lib.rs` files are empty; `crates/bincache/src/main.rs`
is a `clap` parse that prints a greeting. The system described below exists as prose in
`research/DESIGN.md` and the six crate READMEs.

That is worth stating plainly: the architecture is designed about three layers deeper than
it is built, so there is no code to anchor any of it against. This file is the anchor
until there is.

## The one sentence

A narinfo response is fully built at upload time and stored in RAM as finished bytes.
Serving it is a hash lookup plus one write.

Everything else is one of three things:

1. Making that lookup fast (index structure, hasher, filter tier, huge pages).
2. Getting NAR bytes to the socket without copying them (splice, page cache, no O_DIRECT).
3. Making the RAM state rebuildable from disk after a crash (snapshot, log, rescan).

## Three planes

| Plane | Question it answers | Crates |
|---|---|---|
| Metadata | "Do you have path X, and what is it?" | `index`, with bytes from `core` |
| Payload | "Give me the NAR." | `store` opens the fd, `serve` splices it |
| Ingest | "Here is a new path." | `ingest`, driving `core`, `store`, `index` |

The metadata plane is miss-dominated: a client resolving a 500-path closure asks about all
500 and downloads only what we have. The payload plane is hit-only, because a client only
fetches a NAR whose narinfo it already resolved. Those opposite profiles are why the two
planes are engineered separately, and that split is the organizing decision of the design.

## Crates, by ownership

Crates split by who owns what, not by feature.

- **`bincache-core`** is the vocabulary and the pure functions. `StorePathHash`,
  `NarHash`, `Signature`, narinfo render, ed25519 sign, base32 decode. No I/O, no async,
  no sibling deps. The only crate all five others agree on.
- **`bincache-index`** is RAM. Hash to a pointer into a huge-page arena of pre-rendered
  blobs, published through a snapshot pointer.
- **`bincache-store`** is disk. Files, `O_TMPFILE`, `fsync`, `linkat`, the publish log,
  the `rkyv` snapshots. Ground truth.
- **`bincache-ingest`** is the only writer.
- **`bincache-serve`** is the readers. Holds a snapshot pointer and some file descriptors,
  nothing else.
- **`bincache`** is wiring. The only crate that knows the other five exist together.

Dependency direction, which is also the layering:

```
core  <-  index  <-  ingest  <-  bincache
  ^         ^     <-  serve   <-
  |         |
  +-- store +
```

## Trace a request

### `GET /<hash>.narinfo`, the hot path

1. `serve` reads the socket into a pool-owned buffer and parses HTTP/1.1 by hand.
2. `serve` decodes the 32-character base32 key into `core::StorePathHash([u8; 20])`.
   Malformed input dies here with a 400, before touching any data structure.
3. `serve` loads the `index` snapshot pointer with `Acquire`.
4. `index` checks the delta tier, then the frozen tier, yielding `(ptr, len)` into the
   arena. Later, a binary fuse filter short-circuits definite misses before this step.
5. `serve` writes those bytes. One vectored write of an immutable buffer.

No allocation, no formatting, no lock, and no shared cache-line write anywhere in that
list. That absence is the design goal.

`HEAD` reuses the header segment of the same blob.

### `GET /nar/<filehash>.nar.zst`

1. Same parse and decode.
2. `store` performs the `statx`-enriched open and hands over a descriptor.
3. `serve` loops bounded zero-copy sends, yielding to the scheduler between chunks so one
   elephant stream cannot monopolize its shard.

Userspace never sees a NAR byte. The page cache is the payload cache.

### `PUT`, the ingest chain

Encoded as a typestate machine, each transition consuming `self`:

```
Upload<Receiving>   stream into O_TMPFILE, hash incrementally
Upload<Verified>    declared NarHash matched, else abort before anything durable exists
Upload<Compressed>  zstd
Upload<Stored>      fsync, then linkat to nar/<filehash>.nar.zst
Published           core renders and signs the blob; index bump-allocates it into the
                    arena and swaps the snapshot pointer with Release
```

`publish()` exists only on the state whose verify and sign steps have run, so serving
unverified content is a compile error rather than a review catch. A crash at any point
leaves either nothing or an orphan file, which is what makes a client retry a no-op.

### Boot

`bincache` parses config, `store` maps the latest `rkyv` snapshot through the checked API,
`index` builds from it, `store` replays the publish log tail, `serve` starts listeners. A
snapshot that fails validation degrades to an O(n) filesystem rescan: slow boot, no data
loss.

## The two connectors

If you retain nothing else, retain these.

**The pre-rendered response blob.** `core` makes it, `index` stores it in the arena and
points at it, `serve` writes it verbatim, `store` persists the metadata that regenerates
it during key rotation, `ingest` orchestrates the chain. It is the object every crate
touches.

**The snapshot pointer.** The only cross-core communication on the read path: one
`Release` store per publish batch from ingest, an `Acquire` load per request from every
shard. That single atomic is the entire coupling between the write side and the read side.
Everything the shards read is immutable, which is what licenses lock-free reads, epoch
reclamation, and permanent precomputation.

## The unresolved fork

`research/CLAUDE_ROAST_1.md` disagrees with `research/DESIGN.md` on load-bearing points,
and no winner has been picked. Reading both as though they agree is a good way to stay
confused. The live disagreements:

| Question | DESIGN.md | CLAUDE_ROAST_1.md |
|---|---|---|
| Runtime | Compio thread-per-core on io_uring | io_uring is seccomp-blocked by default in Docker 25+ and containerd; start on tokio/epoll |
| What gets pre-rendered | Full HTTP response, headers included | Body only; baked headers break Range, 304, and HTTP/2 |
| Metadata store | Custom frozen plus delta over a huge-page arena | An embedded KV store is adequate; harmonia serves narinfo at 82 microseconds from SQLite |
| The real bottleneck | Server-side per-request fixed cost | Client-side xz decompression and narinfo round-trip concurrency |
| Reclamation | `crossbeam-epoch` | `arc-swap`, given whole-snapshot swap |
| DST | The centerpiece | Not achievable on compio today; schedule risk |

DESIGN.md describes a v3 architecture. The roast describes a v1 that can be finished. The
crate boundaries above survive either answer, so this fork is safe to leave open while
writing `core`, and expensive to leave open past that.

## Where to look

- `research/NIX_PRIMER.md`: what NAR, narinfo, and the store path hash are. Start here if
  the protocol vocabulary is not yet automatic.
- `research/DESIGN.md`: the reasoning, per decision, with the numbers.
- `research/CLAUDE_ROAST_1.md`: the evidence-based challenge to it, plus a staged plan.
- `research/ATTIC_BREAKDOWN.md`: how the closest prior art works and where it hurts.
- `crates/*/README.md`: per-crate owns, does-not-own, and depends-on.
