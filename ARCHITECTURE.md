# bincache: how the parts fit

Orientation doc. Read this to reload the whole system into your head.

## Current state

V1 is implemented and serves the protocol end to end. A `nix copy --to` push lands, a
`GET` of the narinfo returns a signed record, and the NAR streams back and decompresses to
what was uploaded. `crates/bincache-serve/tests/conformance.rs` runs a real server on a
real socket and asserts that; `scripts/conformance.py` does the same against a running
process, including verifying the ed25519 signature the way a client verifies it.

Deferred on purpose: TLS, HTTP/2, and garbage collection.

## The one sentence

A narinfo body and its signature are computed once, at upload time, from fields the server
verified itself, and written to a file. Serving one is an `open` and a `write`.

Everything else is one of three things:

1. Getting NAR bytes to the socket without buffering the whole artifact.
2. Making sure a crash leaves either nothing or an orphan file, never a lie.
3. Making sure what a client uploaded is what a client can verify.

## The data directory

The filesystem is the database. There is no index, no transaction, and no cache in front of
one, because the thing being served is an immutable blob named by a hash, and that is what
a filesystem already is.

```
<data-dir>/
  narinfo/<store path hash>.narinfo   the exact bytes GET returns
  bynar/<nar hash>                    "<file hash> <file size> <nar size>"
  nar/<ab>/<file hash>.nar.zst        the compressed artifact
  staging/                            uploads that have not committed
```

Every write is: write a temporary file, `fsync` it, rename it into place, `fsync` the
directory. The rename is the publish. A reader sees the previous contents or the new ones,
never a partial file, and the directory sync is what makes that survive a power loss rather
than only a process crash.

`bynar` exists because a client declares the hash of the *uncompressed* NAR while bincache
recompresses on receipt, so the artifact is named by a hash the client never computed. It
carries `nar_size` because that is the one field the filesystem cannot answer: it describes
the uncompressed bytes, and a streamed zstd frame does not pledge its content size.

No lock file and no single-writer database, so `reconcile`, `delete`, and `rotate` run
against a live server. A running server picks up an out-of-band delete immediately, because
there is nothing cached to invalidate.

## Two planes

| Plane | Question it answers | Crates |
|---|---|---|
| Metadata | "Do you have path X, and what is it?" | `store` reads the file, `serve` writes it out |
| Payload | "Give me the NAR." | `store` opens the artifact, `serve` streams it |
| Ingest | "Here is a new path." | `ingest`, driving `core` and `store` |

The metadata plane is miss-dominated: a client resolving a 500-path closure asks about all
500 and downloads only what we have. The payload plane is hit-only, because a client only
fetches a NAR whose narinfo it already resolved.

## Crates, by ownership

- **`bincache-core`** is the vocabulary and the pure functions. Nix base32, `hash::Sha256`,
  `storepath::{Hash, Path, Dir}`, the `narinfo::NarInfo` record with its render, parse, and
  fingerprint, ed25519 keys and signatures, and `narurl` as the single owner of the
  `nar/<file hash>.nar<ext>` convention in both directions. No I/O, no async, no sibling
  deps.
- **`bincache-store`** is the data directory: the atomic-write discipline, content-addressed
  artifacts, published narinfo bodies, NAR receipts, and the scan that finds orphans.
- **`bincache-ingest`** is the only writer. The typestate upload machine, the publish, push
  auth, and the maintenance operations.
- **`bincache-serve`** is the readers: the axum router, routing, ranges, counters.
- **`bincache`** is wiring. The only crate that knows the other four exist together.

Dependency direction, which is also the layering:

```
core  <-  store  <-  ingest  <-  serve  <-  bincache
```

## Trace a request

### `GET /<hash>.narinfo`, the hot path

1. hyper parses the request. This crate never sees a byte of HTTP framing.
2. `route` decodes the 32-character base32 key into `core::storepath::Hash`. Malformed
   input dies here with a 400, before touching the filesystem.
3. `store::narinfo` opens `narinfo/<hash>.narinfo` and reads it.
4. `serve` writes those bytes back, unchanged.

`HEAD` runs the same path; hyper suppresses the body and keeps the length.

### `GET /nar/<filehash>.nar.zst`

1. Same parse and decode.
2. `store` opens the file and reports its size.
3. `range` resolves any `Range` header against that size.
4. `serve` seeks and hands hyper a bounded stream over the file.

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
  receipt::Receipt     fsync, rename to nar/<file hash>.nar.zst, write the receipt

PUT /<hash>.narinfo             the publish
  parse, read the receipt, take every payload field from what was received,
  discard the client's signatures, sign, render, write the file
```

Each transition consumes `self`, and `store` exists only on `Upload<Verified>`, so
committing unverified content is a compile error. A crash at any point leaves either
nothing or an orphan artifact, which is what makes a client retry a no-op.

The client must be pointed at the cache with `?compression=none`: bincache verifies the NAR
hash the protocol defines, over the bytes the protocol defines, and compresses on receipt.
A pre-compressed upload is refused before its body is read.

### Boot

Open the data directory, sweep temporary files a crash may have left, count what is
published so `/metrics` has a starting point, and listen. There is no snapshot to validate
and no log to replay: a rename is the publish.

## The two connectors

If you retain nothing else, retain these.

**The verified record.** `ingest` produces it from bytes it hashed itself, `core` renders
and signs it, `store` writes it, `serve` returns those exact bytes. Every field describing
the payload comes from what arrived, never from what the client claimed. A client that
declares the wrong `NarSize` is corrected rather than refused: the `NarHash` it declared
already determines the content, which determines the size.

**The content-addressed name.** `core::narurl` owns it, `store` turns it into a path,
`serve` routes on it, and the `PUT` target carries the hash the body must match. That last
part is what lets verification happen during receive, before anything durable is named.

Because a NAR excludes the store path name, two paths with identical contents share one
artifact. `delete` therefore forgets the record and its receipt and leaves the artifact for
`reconcile` to report, rather than stranding a sibling.

## Where to look

- `research/NIX_PRIMER.md`: what NAR, narinfo, and the store path hash are. Start here if
  the protocol vocabulary is not yet automatic.
- `research/ATTIC_BREAKDOWN.md`: how the closest prior art works and where it hurts.
- `research/DESIGN.md`, `research/DESIGN_V2.md`, and `research/CLAUDE_ROAST_1.md`: the
  design argument as it was had. Superseded in the parts that describe an index, a RAM
  projection, or thread-per-core; see the header on `DESIGN_V2.md`.
