# bincache: how the parts fit

Orientation doc. Read this to reload the whole system into your head, then go to
`research/DESIGN_V2.md` for the reasoning behind any single decision.

## Current state

V1 is implemented and serves the protocol end to end. A `nix copy --to` push lands, a
`GET` of the narinfo returns a signed record, and the NAR streams back and decompresses to
what was uploaded. `crates/bincache-serve/tests/conformance.rs` runs a real shard on a real
socket and asserts that; `scripts/conformance.py` does the same against a running process,
including verifying the ed25519 signature the way a client verifies it.

Deferred on purpose, with the reasoning in `research/DESIGN_V2.md`: the RAM projection in
front of `redb`, TLS, HTTP/2, garbage collection, and the negative-lookup filter tier.
Rendering the narinfo body per request was measured and moved to publish time; the profile
behind that is in this file's history rather than in `DESIGN_V2.md`.

## The one sentence

A narinfo body and its signature are computed once, at upload time, from fields the server
verified itself. Serving one is a key lookup plus a write.

Everything else is one of three things:

1. Making that lookup fast (the KV store now, with the rendered body projected beside each
   record, and a derived RAM projection later).
2. Getting NAR bytes to the socket in bounded chunks without buffering the whole artifact.
3. Making sure a crash leaves either nothing or an orphan file, never a lie.

## Three planes

| Plane | Question it answers | Crates |
|---|---|---|
| Metadata | "Do you have path X, and what is it?" | `index`, rendering with `core` |
| Payload | "Give me the NAR." | `store` opens the file, `serve` streams it |
| Ingest | "Here is a new path." | `ingest`, driving `core`, `store`, `index` |

The metadata plane is miss-dominated: a client resolving a 500-path closure asks about all
500 and downloads only what we have. The payload plane is hit-only, because a client only
fetches a NAR whose narinfo it already resolved. Those opposite profiles are why the two
planes are engineered separately, and that split is the organizing decision of the design.

## Crates, by ownership

Crates split by who owns what, not by feature.

- **`bincache-core`** is the vocabulary and the pure functions. Nix base32, `hash::Sha256`,
  `storepath::{Hash, Path, Dir}`, the `narinfo::NarInfo` record with its render, parse, and
  fingerprint, ed25519 keys and signatures, and `narurl` as the single owner of the
  `nar/<file hash>.nar<ext>` convention in both directions. No I/O, no async, no sibling
  deps.
- **`bincache-index`** is the `redb` schema. Three tables: the records, the bodies they
  render to, and the NAR entries.
- **`bincache-store`** is the filesystem: content-addressed artifacts, atomic appearance,
  reader handles, and the scan that finds orphans.
- **`bincache-ingest`** is the only writer. The typestate upload machine, the publish, push
  auth, and the maintenance operations.
- **`bincache-serve`** is the readers: shards, HTTP/1.1, routing, ranges, counters.
- **`bincache`** is wiring. The only crate that knows the other five exist together.

Dependency direction, which is also the layering:

```
core  <-  index  <-  ingest  <-  serve  <-  bincache
  ^         ^     <-  store   <-      <-
  |         |
  +-- store +
```

## Trace a request

### `GET /<hash>.narinfo`, the hot path

1. `serve` reads the socket into the connection's buffer and parses HTTP/1.1 by hand.
2. `route` decodes the 32-character base32 key into `core::storepath::Hash`. Malformed
   input dies here with a 400, before touching any data structure.
3. `index` reads the body out of `redb`, already rendered. A record with none projected
   beside it renders on the spot, which is what an index written by an older build gets.
4. `serve` builds the framing and writes head and body in one buffer.

`HEAD` renders the same body, reports its length, and writes no body. That is the whole
reason the stored artifact is a *body* rather than a framed response: HTTP/2 becomes a
change to `serve` rather than a data migration.

### `GET /nar/<filehash>.nar.zst`

1. Same parse and decode.
2. `store` opens the file and reports its size.
3. `range` resolves any `Range` header against that size.
4. `serve` loops bounded reads and writes through one reused buffer, handing the shard back
   to its scheduler between chunks so one elephant stream cannot monopolize it.

Every NAR response carries `Accept-Ranges: bytes` and no `Content-Encoding`. Both are
required for a dropped transfer to resume rather than restart, and both are asserted by a
conformance test.

### `PUT`, the ingest chain

A client pushes the payload first, then the metadata.

```
PUT /nar/<nar hash>.nar         the target states what the body must hash to
  Upload<Receiving>    stream in, hash the NAR, zstd-compress toward staging
  Upload<Compressed>   encoder flushed; both hashes and both sizes final
  Upload<Verified>     the computed hash matched the target, else abort
  nar::Entry           fsync, rename to nar/<file hash>.nar.zst, record it

PUT /<hash>.narinfo             the publish
  parse, look up the NAR entry, take every payload field from what was received,
  discard the client's signatures, sign, render, commit
```

Each transition consumes `self`, and `store` exists only on `Upload<Verified>`, so
committing unverified content is a compile error. A crash at any point leaves either
nothing or an orphan artifact, which is what makes a client retry a no-op.

The client must be pointed at the cache with `?compression=none`: bincache verifies the NAR
hash the protocol defines, over the bytes the protocol defines, and compresses on receipt.
A pre-compressed upload is refused with a message naming the setting.

### Boot

Open the payload directory and the `redb` file, sweep staging files a crash may have left,
start the watchdog, start the shards. There is no snapshot to validate and no log to
replay: a `redb` commit is the publish.

## The two connectors

If you retain nothing else, retain these.

**The verified record.** `ingest` produces it from bytes it hashed itself, `core` renders
and signs it, `index` stores the record and the body together in one commit, `serve` copies
that body to the socket. Every field describing the payload comes from what arrived, never
from what the client claimed.

**The content-addressed name.** `core::narurl` owns it, `store` turns it into a path,
`serve` routes on it, and the `PUT` target carries the hash the body must match. That last
part is what lets verification happen during receive, before anything durable is named.

## Where to look

- `research/NIX_PRIMER.md`: what NAR, narinfo, and the store path hash are. Start here if
  the protocol vocabulary is not yet automatic.
- `research/DESIGN_V2.md`: the reasoning, per decision, with the numbers, plus what V1
  actually shipped and where it departs from the design.
- `research/DESIGN.md` and `research/CLAUDE_ROAST_1.md`: the argument DESIGN_V2 answers.
- `research/ATTIC_BREAKDOWN.md`: how the closest prior art works and where it hurts.
- `crates/*/README.md`: per-crate owns, does-not-own, and depends-on.
