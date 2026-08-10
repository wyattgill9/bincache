---
tags:
  - rust
  - architecture
  - performance
  - concurrency
  - nix
supersedes: research/DESIGN.md
inputs:
  - research/DESIGN.md
  - research/CLAUDE_ROAST_1.md
  - NixOS/nix @ 7362ff0 (2.36.0), read for client behaviour
last_updated: 2026-07-30
---

# bincache: Design v2

This document replaces `research/DESIGN.md`. It keeps the decisions from v1 that survived
`research/CLAUDE_ROAST_1.md`, reverses the ones that did not, and records the reversals
with the evidence that forced them. v1 stays on disk as the argument this one answers.

## Charter, stated honestly

bincache is a performance-research vehicle. That is the whole claim, and it is worth being
precise about what it is not: it is not a response to an unmet need. Harmonia serves
narinfo at 82 µs reading straight from SQLite and streams NARs at multi-GiB/s. Nix-serve-ng
fetches NARs about 30× faster than nix-serve. Both already exceed what any Nix client can
consume. Nobody is blocked on a faster origin.

bincache exists to find where the ceiling actually sits for this workload when you spend
thread-per-core, io_uring, and kernel zero-copy on it. Two obligations follow from choosing
that framing on purpose:

1. **Correct against real clients first.** A cache that `nix build` cannot substitute from
   is not a fast cache, it is a fast nothing. Protocol conformance gates every optimization.
2. **Every optimization carries a before/after number.** Research is measurement. An
   unmeasured optimization in this codebase is a bug in the charter.

## Where the time actually goes

v1 organized itself around server-side per-request fixed cost. The evidence says that is
not what a user feels. Recording it here so no section below can quietly forget it:

- **Client decompression is single-threaded and CPU-bound.** nh2's gdb backtrace on
  NixOS Discourse shows `nix` burning 70–130% of a core inside `lzma_decode()` while
  pulling only ~12 MB/s. Nix issue #12355 confirms there is no parallel decompression
  client-side. zstd decompresses roughly 10× faster than xz for about 23% more bytes.
- **Narinfo round-trips serialize.** Nix issue #5118 measures closure querying at 38 s
  with concurrency 32 and 0.73 s at concurrency 1000. A one-line concurrency change is
  worth ~50×.
- **Server-side narinfo headroom is small.** Gonzalez's own nix-serve-ng table shows
  ~1.5–1.8× improvement from a full rewrite, against ~30× for NAR fetching.

So the two decisions in this document that a real user would notice are **zstd** and
**request concurrency**. Everything else is ceiling exploration and is labeled as such.
That labeling is the honest version of v1's performance argument, not a retreat from it.

## Decision log

| Question | v1 said | v2 says |
|---|---|---|
| Purpose | Fastest read-optimized cache | Performance-research vehicle, stated as such |
| Deployment | Bare-metal pet box | Unchanged: standalone bare-metal origin, TLS in-process |
| Runtime | Compio thread-per-core on io_uring | Unchanged, with the seccomp cost accepted explicitly |
| Metadata ground truth | Custom frozen+delta over a huge-page arena | Embedded KV store (`redb`); RAM tier is a later projection |
| Reclamation | `crossbeam-epoch` | `arc-swap`, revisit only on measured guard cost |
| Compression | ZSTD | ZSTD only, with Nix 2.4+ declared as a client floor |
| Push protocol | HTTP `PUT` | `PUT` plus bearer tokens for v1; S3 API later |
| HTTP version | Hand-rolled HTTP/1.1 | Unchanged for v1, keep-alive tuned; h2 deferred but first in the queue |
| TLS | kTLS offload | Userspace `rustls`; kTLS deferred until it proves out |
| Accept model | `SO_REUSEPORT` per shard | Unchanged; connection skew accepted as a known issue |
| Testing | DST as the centerpiece | Deferred. `loom` plus property tests; the seam is kept |
| GC | Closure-aware eviction planner | No GC in v1. Mechanism only, operator-triggered delete |
| Scale target | Unstated | Hard ceiling of ~10M paths for the RAM tier |
| Baselines | Unstated | Skeleton first, harmonia/nginx comparison after |
| Prerender scope | Full HTTP response, headers included | Body only, framing built per request. See `What gets prerendered` |

## Protocol surface (the part v1 never wrote down)

v1 assumed the Nix protocol and never specified it. Since conformance now gates
everything, it goes first.

**Required reads:**

- `GET /nix-cache-info` returning `StoreDir`, `WantMassQuery`, `Priority`.
- `GET` and `HEAD /<hash>.narinfo`, Content-Type `text/x-nix-narinfo`. A real body is
  around 1 KB (cache.nixos.org's `ruby-2.7.3` narinfo is 1058 bytes).
- `GET /nar/<filehash>.nar.zst`, Content-Type `application/x-nix-nar`.

**narinfo fields:** `StorePath`, `URL`, `Compression`, `FileHash`, `FileSize`, `NarHash`,
`NarSize`, `References`, `Deriver`, `Sig`, `CA`. `FileHash` and `FileSize` describe the
*compressed* file, not the NAR. Exactly four are hard-required: `NarInfo::NarInfo` in
`src/libstore/nar-info.cc` throws `corrupt` unless `StorePath`, `NarHash`, and `URL` are
present and `NarSize` is nonzero. Everything else parses fine by its absence, which makes
those four the conformance floor rather than the full field list.

**Signing:** `Sig` is `<key-name>:<base64 ed25519 signature>` over the standard
fingerprint string built from `StorePath`, `NarHash`, `NarSize`, and sorted `References`.
Multiple `Sig` lines are legal and a client accepts the path if *any* signature matches a
key in its `trusted-public-keys`. That is what makes key rotation non-disruptive: publish
the new public key to clients, then switch the signing key.

### Client behaviours that constrain the design

Read out of the Nix 2.36.0 tree rather than recalled, because every one of these decides
something below. `filetransfer.cc` and `http-binary-cache-store.cc` are the two files.

- **Decompression is chosen from the narinfo `Compression` field, not from HTTP
  `Content-Encoding`.** Transparent HTTP compression is the wrong lever and also breaks
  resumption.
- **An absent or empty `Compression` field means bzip2, not none.** `nar-info.cc` defaults
  it that way and calls the conditional that produces it a mistake in its own comment.
  Always emit `Compression: zstd` explicitly; omitting it does not mean what it looks like.
- **Clients cache narinfo results in a local SQLite disk cache** with a **positive TTL of
  30 days** and a **negative TTL of 3600 s**, both floors that `--refresh` cannot lower
  (`nar-info-disk-cache.cc` clamps with `std::max`). A warm client does not re-ask. This is
  why the negative-lookup filter tier below is demoted: it only earns its keep during
  cold-cache stampedes, not in steady state.
- **`HEAD` hits both endpoints, not just narinfo.** `isValidPathUncached` HEADs
  `<hash>.narinfo`, and the upload path HEADs the NAR `URL` to skip re-uploading a payload
  that already exists. Only the status code is read, since curl is in `NOBODY` mode.
- **No conditional requests, ever.** `expectedETag` is set only by the tarball fetcher in
  `libfetchers`. The binary cache store never sends `If-None-Match`, so `ETag` and 304 are
  dead weight on both endpoints.
- **`Range` is used only to resume a NAR mid-stream.** `CURLOPT_RESUME_FROM_LARGE` emits a
  single open-ended `Range: bytes=N-`, and `maybeRetry` resumes only when the original
  response carried `Accept-Ranges: bytes` *and* no `Content-Encoding`. Narinfo bodies are
  buffered into a string rather than streamed to a sink, so `writtenToSink` stays zero and
  that branch is unreachable for metadata.
- **curl requests transparent compression on every download.** `CURLOPT_ACCEPT_ENCODING`
  is set to `""`, meaning every encoding the linked libcurl supports. Answering a NAR with
  `Content-Encoding` therefore double-encodes against the narinfo `Compression` contract
  *and* silently disables resume.
- **The client negotiates HTTP/2 by default over TLS.** The `http2` setting defaults true,
  `CURLOPT_HTTP_VERSION` is `CURL_HTTP_VERSION_2TLS`, `CURLOPT_PIPEWAIT` is set, and the
  multi handle enables `CURLPIPE_MULTIPLEX`. `http-connections` (default 25) caps TCP
  connections, not in-flight requests, so over h2 one connection carries an entire closure
  query. `2TLS` also means plaintext `http://` stays HTTP/1.1: there is no h2c upgrade to
  support.

**Explicitly not in v1:** `.ls` file listings, `log/<drv>` build logs, `debuginfo/`, and
`realisations/<drvOutput>.doi` for content-addressed derivations. Harmonia implements
them; none are needed to substitute a closure.

## The two workloads

The organizing split from v1 survives intact, because it is a fact about the protocol
rather than a bet about implementation.

| | Metadata (`.narinfo`, `HEAD`, `nix-cache-info`) | Payload (NAR) |
|---|---|---|
| Size | ~300–1000 B | KB to GB |
| Rate | Bursty: closure resolution fires hundreds of lookups at once | Sustained streams |
| Bound by | Per-request fixed cost and round-trip count | NIC and storage bandwidth |
| Miss rate | High: clients probe for paths the cache may not have | ~Zero: clients only fetch NARs they already resolved |

Metadata lookups are miss-dominated, payload fetches are hit-only. The planes are
engineered separately for that reason and no other.

## System shape

```
                       ┌────────────────────────────────────────────────┐
                       │  serving shards (one per core, pinned)         │
   NIC (RSS/REUSEPORT) │  ┌──────────┐ ┌──────────┐      ┌──────────┐   │
   ────────────────────┼─▶│ shard 0  │ │ shard 1  │ ...  │ shard N  │   │
                       │  └────┬─────┘ └────┬─────┘      └────┬─────┘   │
                       │       │ arc-swap guard, read-only              │
                       │  ┌────▼─────────────▼─────────────────▼─────┐  │
                       │  │  metadata index                          │  │
                       │  │  redb  (ground truth, always)            │  │
                       │  │  RAM projection (later, derived, ≤10M)   │  │
                       │  └────▲─────────────────────────────────────┘  │
                       │       │ publish (KV commit, then swap)         │
                       │  ┌────┴─────────────────────────────────────┐  │
   PUT (token) ────────┼─▶│  ingest: verify → compress → store → sign│  │
                       │  └────┬─────────────────────────────────────┘  │
                       │       ▼                                        │
                       │   filesystem: nar/<filehash>.nar.zst           │
                       └────────────────────────────────────────────────┘
```

Shared-nothing for connections and mutable state; shared-everything and read-only for the
metadata index. The ingest pipeline is the only writer.

## Execution model: thread-per-core on Compio

**Decision: [[compio]], one shard per physical core, [[core-pinning|pinned]],
`SO_REUSEPORT` listeners per shard.** Unchanged from v1, and the reason it survives the
roast is the charter: exploring this runtime *is* the project.

Compio over the alternatives, per [[shard-per-core-runtimes-compared]]: [[glommio]] is
effectively unmaintained and carries an open memory-corruption report in
`channels::spsc_queue` from December 2025 (issues #700/#701). [[monoio]] has a better
slab-based allocation story but narrower io_uring coverage. `tokio-uring` has not shipped
a meaningful release since 2022. [[apache-iggy]] migrated Tokio to Compio in v0.6.0 and
reports >5 GB/s with fsync-per-message, which is a strictly harder durability regime than
serving read-only files.

Compio's known cost is that it boxes every I/O request. Iggy found [[mimalloc]]'s
small-allocation pool absorbs it, and the Compio authors declined a slab allocator. Note
also that Compio's own docs warn `send_zerocopy` "is not always faster than send", so the
zero-copy claims in the payload plane are hypotheses to measure, not results.

### The io_uring deployment cost, accepted rather than argued away

io_uring is blocked by default in Docker 25.0.0+ (moby PR #46762) and in containerd's
`RuntimeDefault` seccomp profile (PR #9320), which Kubernetes inherits. Google reported in
June 2023 that io_uring accounted for 60% of kCTF kernel exploit submissions and roughly
$1M in bounties, and subsequently disabled it across ChromeOS, Android, and production.
TigerBeetle issue #1995 shows the user-visible result: `PermissionDenied` at startup with
no fallback.

The consequence is a hard deployment requirement, stated as policy rather than discovered
as a bug: **bincache runs on bare metal, or in a container with a custom seccomp profile
that permits `io_uring_setup`, `io_uring_enter`, and `io_uring_register`.** Default-profile
containers are unsupported. That is a real cost of the research framing and it is being
paid knowingly.

**The fallback stays named:** N independent `current_thread` Tokio runtimes with
`SO_REUSEPORT` preserves every other decision in this document and gives up only the
io_uring-specific wins. The runtime boundary is kept thin so the swap stays cheap.

### io_uring hazards

- **All I/O buffers are pool-owned.** Buffers come from a per-shard pool with stable
  addresses through Compio's registered-buffer support. A dropped operation returns its
  buffer at completion, so the kernel never holds a pointer into freed memory.
- **No naked `select!` around in-flight I/O.** Timeouts use ring-native linked-timeout
  operations so cancellation is kernel-visible. Enforced by keeping raw ops private to one
  module.
- **Head-of-line blocking.** Compio has no [[glommio]]-style stall detection. NAR streams
  are chunked sends that re-enter the scheduler between chunks. A per-shard heartbeat
  timestamp written [[memory-ordering|relaxed]] and read by a watchdog thread gives
  stall detection with a stack dump on `SIGUSR1`.
- **`RefCell` across `.await`.** The failure Iggy hit. Per-shard state is decomposed
  struct-of-arrays style so no borrow spans a yield point.

### Accept model, and the skew it causes

`SO_REUSEPORT` hashes connections to shards at accept time and never rebalances. With
long-lived keep-alive connections and multi-GB NAR streams, one CI machine pulling a large
closure pins its stream to a single shard while others idle.

**This is accepted as a known issue, not mitigated in v1.** The mitigations exist (eBPF
reuseport socket selection, or a shared accept queue for the payload plane only) and both
are recorded in the risk register. Taking the skew keeps the shared-nothing property that
the research is about. Revisit when it is measured, not before.

### Pinning and topology

Each shard pins to one physical core; NIC IRQ affinity aligns RX queues to serving cores.
SMT stays **on**: [[core-pinning]] is explicit that disabling it is the HFT tail-latency
recipe, and throughput-oriented servers gain from the extra logical threads. No `isolcpus`
by default, since it buys P99.9 at roughly 4× overprovisioning cost, which is the wrong
trade for a bandwidth-bound cache. Both stay documented knobs.

## The metadata plane

This is where v2 diverges hardest from v1. v1 made a custom in-RAM frozen-plus-delta
structure over a huge-page arena the ground truth. v2 makes an embedded KV store the
ground truth and demotes the RAM structure to a derived projection that does not exist yet.

### Ground truth: an embedded KV store

**Decision: [[redb]] holds the canonical `StorePathHash → narinfo record` mapping.**
Values are [[rkyv]]-encoded records, not rendered bytes, so the render format can change
without a data migration.

The argument v1 made against embedded stores was that a network round-trip to storage is
10³–10⁴× an in-RAM lookup. That argument is correct and irrelevant: an *embedded* store is
not a network hop, and harmonia hits 82 µs reading narinfo straight from SQLite with the
OS page cache doing the work. 82 µs is already three orders of magnitude under what any
client notices.

What the KV store buys, all of which v1 had to build by hand:

- Crash-safe atomic writes, so `Durability and Recovery` shrinks to almost nothing.
- No custom on-disk format, no publish log, no `rkyv` snapshot file, no O(n) filesystem
  rescan as the corruption floor.
- No "rebuild the perfect-hash index on every mutation" problem, which is the structural
  reason v1's design fit a read-only corpus and bincache is not one. It accepts writes.
- No torn-write soundness question. Mmap'ing an `rkyv` archive while a writer rewrites it
  in place is unsound; `rkyv`'s `access_unchecked` skips validation and is unsafe on
  malformed data, while checked `access` costs a validation pass proportional to archive
  size. Both problems disappear when the store owns durability.

`fjall` is the named alternative if write throughput ever dominates, since an LSM absorbs
bursts better than a B-tree. That would be a measured swap behind the same `Index` trait.

### What gets prerendered

This was the last open decision in the document. **Resolved: the prerendered artifact is
the narinfo body. Framing is built per request.** v1 baked whole HTTP responses, headers
included; that is reversed.

The argument is a straight cost/benefit once the client behaviours above are on the table.

*What baking headers would buy.* A narinfo response's headers are a pure function of the
body: a status line, `Content-Type: text/x-nix-narinfo`, and `Content-Length`. Nothing
varies per request. Nothing else in the response is read on this endpoint, because the
client sends no `If-None-Match` (so `ETag` and 304 are unreachable), sends no `Range` on
metadata (so `Accept-Ranges` is unreachable), and buffers the body rather than streaming
it. So concatenating the header prefix into the stored blob saves exactly **one iovec
entry**. Under a vectored write that is the same one syscall either way. The measured win
is zero, and it was never anything else; v1 simply never priced it.

*What baking headers would cost.* HTTP/1.1 framing, permanently, in the durable
representation. The client negotiates h2 by default over TLS and bincache terminates TLS
in-process, so **every real client of this cache will be speaking h2**, where a baked
`HTTP/1.1 200 OK\r\n...` prefix is not a prefix of anything. h2 is also the highest-value
deferred item in this document, since multiplexing is what answers issue #5118 and it is
purely a server-side change. Baking h1 framing would mean paying a data migration to
collect the largest available win.

Zero benefit against a self-inflicted block on the best remaining optimization is not a
close call. The hybrid that was also on the table, storing the body alongside a cached
HTTP/1.1 header prefix for the plain-GET fast path, is rejected for the same reason: it
buys the same nothing and adds a second representation to keep coherent.

So: `bincache-core` renders a body, `bincache-index` stores and publishes bodies, and
`bincache-serve` owns framing entirely. `HEAD` reuses the body's length and writes no body.
The protocol version stops being visible to the storage layer, which is what keeps h2 a
serving-crate change rather than a schema change.

The residue of v1's idea survives and is the part that mattered: the expensive work
(rendering, signing) still happens once at ingest, and the read path still writes immutable
bytes it did not allocate or format. Only the boundary of "what is a blob" moved, from the
response to the body.

### RAM tier: a projection, added later, with a ceiling

When profiling shows the KV read path is hot, a RAM tier lands **as a strictly derived
read-only projection**. It is never authoritative. A corrupt or stale projection is always
recoverable by rebuilding from `redb`, which means the projection needs no durability
story of its own. That single-source-of-truth property is what v1 gave up by making RAM
canonical.

**Hard ceiling: ~10M paths.** The arithmetic, which v1 never did: a rendered narinfo is
~1–1.5 KB, call it ~1.5–2 KB with any header material. 1M paths is ~2 GB, 10M paths is
~15–20 GB. Beyond that the projection stops fitting a sane box. For scale, cache.nixos.org
was 705 TB across more than a billion objects by week 32 of 2025 and grows ~280 GB/day.
**Serving a corpus of that class is explicitly out of scope**, and always was; v1 simply
never said so.

Publication uses **[[arc-swap]]**, not [[crossbeam-epoch]]. The write pattern is one
writer swapping a whole snapshot on a batch cadence, which is the textbook arc-swap case.
Epoch reclamation is warranted for fine-grained concurrent structure mutation, which this
design specifically avoids. If the reader-side guard ever shows up in a profile, epoch
reclamation is the named upgrade behind the same trait boundary.

### Things v1 oversold, corrected

- **Hasher choice barely matters here.** The [[fastest-hash-map-2025]] finding that the
  hasher dominates the table is real for general keys. bincache's keys are already-uniform
  20-byte content hashes, so any non-cryptographic hasher is fine. Use [[foldhash]] and
  stop discussing it. Hash flooding remains a non-issue because insertions come only from
  the authenticated push path.
- **Perfect hashing is deferred, possibly permanently.** PtrHash at 2.4 bits/key (SEA
  2025) is real and best in class, and it is an MPHF that must be rebuilt whenever the key
  set changes. That is a poor fit for a store that accepts writes continuously. It becomes
  interesting only for an offline-rebuildable read-only corpus, which bincache is not.
- **The binary fuse filter is demoted.** `BinaryFuse8` at ~9 bits/key with three memory
  accesses is accurate as described, but clients cache negative answers for 3600 s, so the
  filter earns its keep only under cold-cache stampedes. It lands after the RAM tier, not
  before, and only with a measurement behind it.
- **Huge pages are late-stage tuning.** 2 MB pages materially expand TLB reach and remove
  a class of tail outliers. They are not a design concern and do not belong in an
  architecture document until there is an arena to put on them.

### Parse, don't validate, at the socket

Unchanged and still correct. The request key is a fixed-width 32-character base32 store
path hash, decoded at the boundary into a `StorePathHash([u8; 20])`
[[newtype-pattern|newtype]]. Malformed requests die with a 400 before touching any data
structure, and everything downstream operates on a fixed 20-byte key that is correct by
construction. On a public endpoint the decode doubles as input validation.

## The payload plane

**The page cache is the payload cache.** NARs are stored as individual immutable files at
content-addressed paths (`nar/<filehash>.nar.zst`) and served with kernel zero-copy through
Compio's splice coverage. The serving loop is a `statx`-enriched open followed by bounded
sends with a scheduler yield between chunks, so one elephant stream cannot monopolize its
shard.

- **Compression: zstd only.** This is the decision a user actually feels, per the evidence
  section. The cost is a stated client floor of **Nix 2.4 (April 2021) or newer**; older
  clients cannot decompress zstd and will fail to substitute. No transcoding tier, no dual
  storage, one NAR per path. If xz support is ever needed it arrives as a lazily
  transcoded second artifact, and that is a v2 problem with a real concurrency question
  attached.
- **`Accept-Ranges: bytes` is mandatory here, and `Content-Encoding` is banned here.** Both
  follow from `maybeRetry`: once a NAR download has streamed any bytes to the sink, the
  client can only retry a transient failure by resuming, and it refuses to resume unless
  the first response advertised byte ranges with no content encoding. Get either wrong and
  a dropped connection 9 GB into a 10 GB NAR restarts at zero instead of resuming, which
  presents as a mysterious throughput cliff and never as an error. The Range support
  actually exercised is the single open-ended `bytes=N-` that `CURLOPT_RESUME_FROM_LARGE`
  emits; `bytes=N-M` costs nothing extra to serve from the same code, and multi-range
  requests get a plain 200 with the whole body, which RFC 9110 permits. A 206 carries
  `Content-Range` and the partial `Content-Length`.
- **TLS: userspace [[rustls]].** kTLS is deferred until there is proof it improves
  anything. The entire public demonstration of the io_uring + kTLS + Rust stack is one
  April 2025 hobbyist blog that required upstreaming two PRs to the `ktls` crate, ran no
  benchmarks, and warns the code needs work. TLS 1.3 key updates and renegotiation are the
  classic kTLS footguns, and NIC offload availability is inconsistent. The honest
  consequence: **with userspace TLS the CPU touches every payload byte**, so the zero-copy
  path is fully realized only for plaintext. That is accepted, and closing the gap is
  gated on a spike with numbers.
- **No `O_DIRECT` on the serving path.** Direct I/O is for engines that manage their own
  caching, and this design makes the kernel the payload cache manager on purpose. Where it
  *does* apply: background integrity scans, which would otherwise evict the hot set.
  `POSIX_FADV_DONTNEED` on cold maintenance reads.
- **No [[kernel-bypass|DPDK-class bypass]].** Full bypass surrenders the kernel networking
  stack for a latency win that matters at HFT scale. The bottleneck here is bandwidth.
- **Storage format: plain files, flat directory sharding.** The filesystem is the payload
  database. Packing small NARs into archives is a real future win, deferred until file
  count pressure is measured.

## The ingest plane

Ingest is authenticated, latency-tolerant, and the place where every expensive computation
happens once. It runs on dedicated core(s) so compression and signing never steal cycles
from serving shards.

```
receive ──▶ verify ──▶ compress ──▶ store ──▶ sign ──▶ publish
 (temp)     (hash)      (zstd)     (linkat)   (ed25519)  (redb commit)
```

Encoded with the [[typestate-pattern]]: `Upload<Receiving> → Upload<Verified> →
Upload<Compressed> → Upload<Stored> → Published`, each transition consuming `self`.
`publish()` exists only on the state whose verify and sign steps have run, so serving
unverified content is a compile error rather than a review catch.

Mechanically: bytes stream into an `O_TMPFILE` on the target filesystem while the NAR hash
is computed incrementally, and a mismatch against the declared `NarHash` aborts before
anything durable exists. zstd compression follows. `fsync`, then `linkat` to the final
content-addressed name, which is atomic appearance in the filesystem. The narinfo record
is signed and committed to `redb`, and that commit is the publish. A crash at any point
leaves either nothing or an orphan NAR file, so a client retry is a no-op. Idempotency by
construction.

### Wire format and auth

**v1 write path: HTTP `PUT` with per-node bearer tokens.** Stock `nix copy --to
https://...` does issue `PUT` (confirmed in Nix's `http-binary-cache-store.cc`:
`req.method = HttpMethod::Put`), so this works with zero client tooling, which is the
adoption lever worth keeping. The caveats are real and accepted: Nix's HTTP store has no
locking (its own source notes caches are "inherently racy since there is no locking"), no
multipart, and no dedup. Content addressing plus idempotent publish makes concurrent
pushes of the same path harmless, which covers the race that actually occurs.

Tokens are compared in constant time against an in-memory set. There is no auth on the
read path at all, so the auth check structurally cannot become a read-path dependency.

**S3-compatible ingest is the named next step**, since it is what cache.nixos.org, niks3,
and most serious deployments speak, and it brings multipart upload with it. One canonical
ingest pipeline behind both surfaces.

### Managed signing

The ed25519 key lives only on the server and narinfos are signed at ingest, so a
compromised build node can poison only what it uploads and can never forge signatures for
arbitrary paths. Attic uses the same model. Because narinfo records are stored as fields
rather than rendered bytes, re-signing the world under a new key is a background pass over
`redb`, and clients see no interruption as long as both public keys are in
`trusted-public-keys` during the window.

### Functional core, imperative shell

Verification, narinfo rendering, and signing are pure functions per
[[functional-core-imperative-shell]]. The shell (sockets, files, KV commits) is thin and
lives at the edges. This is what makes the pipeline unit-testable without mocks and
property-testable, and it is the seam that keeps DST possible later without committing to
it now.

## Memory, allocation, and ordering discipline

- **Global allocator: [[mimalloc]].** It specifically absorbs Compio's per-operation
  boxing, which Iggy measured. The commonly quoted figures (5.3× on small allocations,
  13–22% whole-program, ~50% RSS) are workload-specific vendor benchmarks and must be
  re-measured on this workload before being repeated. [[tikv-jemallocator]] is the named
  alternative if long-horizon fragmentation bites.
- **[[false-sharing|128-byte alignment]] for anything per-shard a foreign thread reads:**
  stats counters, heartbeat words. Not 64, because the adjacent-cache-line prefetcher
  makes 64-byte padding insufficient. `crossbeam_utils::CachePadded` throughout.
- **Ordering: acquire/release only, written portably from day one.** `SeqCst` is banned
  absent a written justification. Graviton is a plausible deployment target and
  TSO-implicit code is exactly what breaks there.
- **No cross-core communication on the serving path.** The only standing channels are
  ingest to shards (the arc-swap publish) and shards to watchdog (relaxed heartbeats). If
  ingest needs internal fan-out for parallel compression, the tool is one [[rtrb|SPSC
  ring]] per worker, never an MPMC channel where topology allows SPSC.

## Durability and recovery

Two artifacts persist, and this is much smaller than v1 because `redb` owns the hard part:

1. **NAR files**, content-addressed, immutable, fsync'd before the publish commit.
2. **The `redb` database**, holding every narinfo record.

Boot is: open the database, start listeners. There is no snapshot to validate, no log to
replay, and no O(n) rescan floor. A background scan reconciles the two artifacts, deleting
orphan NARs with no record and flagging records with no NAR.

bincache remains **crash-only software**: there is no orderly-shutdown state to lose,
`panic = "abort"` is safe, and `kill -9` is an acceptable stop mechanism. Immutability is
the enabler, since recovery never has to answer "which version", only "present or absent".

Backup is an `rsync` of the NAR tree plus a `redb` copy, because everything else is
derivable.

## Retention

**No garbage collection in v1.** The cache grows until an operator deletes something.

The mechanism is defined and proven by operator-triggered delete, so a policy can land on
top later without rearchitecting: remove the record from `redb`, drop it from the RAM
projection on the next swap, then unlink the NAR once no snapshot references it. In-flight
NAR streams hold their file descriptor, so an unlinked file finishes streaming safely and
POSIX does the reference counting.

Automatic closure-aware GC is deliberately out of v1. Getting it right means walking
`References` edges, treating recently-served paths as roots, evicting only complete
unreferenced subgraphs, and doing all of that without racing concurrent pushes. Attic,
niks3, and narwal all exist partly because this is hard. It gets designed when it is
built, not before.

One constraint to preserve for whenever that happens: **access tracking must not touch the
read path.** A shared atomic bump per request puts MESI traffic on the hottest path in the
system. Per-shard padded sample buffers with relaxed writes, aggregated lazily, and
approximate recency is entirely sufficient for cache eviction.

## Observability

- **[[tracing]]** with `max_level_info` compile-time gating. Ingest is fully instrumented
  with spans and [[snafu]]-structured errors, where per-stage context is the point. The
  serving path gets counters, not spans.
- **Per-shard [[false-sharing|cache-padded]] counters** (requests, hits, misses, bytes,
  latency buckets) written with relaxed stores by the owning shard only, harvested by the
  metrics endpoint.
- **The watchdog** doubles as the stall detector, covering Compio's biggest operability
  gap versus [[glommio]].

## Testing

**DST is deferred.** v1 made deterministic simulation the centerpiece and a Compio
selection criterion. Iggy's engineers found that with async Rust "it is very difficult, if
not borderline impossible, to achieve total determinism" because the executor's time wheel
and scheduler are not replaceable, and Compio is not pluggable enough today to intercept
them. Building that is original R&D, and it is not what this project is for.

What ships instead:

- **Property tests** ([[hegel]], per the repo rule) on the pure core: narinfo render/parse
  round-trips, base32 decode, signature verify.
- **Protocol conformance tests, since conformance gates everything.** Each client
  behaviour recorded above becomes an assertion: a NAR response advertises
  `Accept-Ranges: bytes` and carries no `Content-Encoding`; a connection dropped mid-NAR
  is resumed by `Range: bytes=N-` rather than restarted; `HEAD` answers on both endpoints;
  a narinfo omitting any of `StorePath` / `NarHash` / `URL` / nonzero `NarSize` is caught
  here rather than by a client's `corrupt` error. The end-to-end version is a real
  `nix build` substituting a closure, which is the only thing that actually proves it.
- **[[loom]]** on the lock-free pieces, which is the narrow slice that actually needs
  schedule exploration.
- **Fault injection against the imperative shell**: torn uploads, disk-full mid-pipeline,
  crash between `linkat` and commit. Deterministic in the sense that the injected fault is
  chosen, not in the sense that the scheduler is reproduced.
- **Exact structural asserts.** `pretty_assertions` for scalars, `expect_test` for
  multi-line output, `insta` for large nested diffs.
- **[[cargo-nextest]]** as the runner, `cargo-deny` on the supply chain, since a service
  holding a signing key does not get to skip dependency auditing.

The functional-core seam is retained specifically so full DST stays reachable if Compio's
driver becomes interceptable.

### Baselines

Comparison against harmonia, nix-serve-ng, and nginx serving a static NAR tree happens
**after** the walking skeleton exists, not before it. The Stage 0 gate the roast proposes
is a gate on whether bincache should exist, and the charter already answers that. The
numbers are for measuring against, and the breakdown that matters is `nix build`
wall-clock split into narinfo resolution, NAR download, decompression, and local insertion.

## Build and packaging

Workspace layout is unchanged, and the crate boundaries survive every reversal above:

```
crates/
  bincache-core/     # pure: types, parsing, narinfo render, signing  (no I/O, no async)
  bincache-index/    # redb schema, arc-swap publication, RAM projection (later)
  bincache-store/    # filesystem layout, NAR files, orphan reconciliation
  bincache-ingest/   # pipeline (typestate), auth
  bincache-serve/    # shards, HTTP/1.1, zero-copy send
  bincache/          # facade binary: config, boot, wiring
```

`bincache-core` having no I/O dependency is load-bearing: it is the property-testable
functional core. Syscall surface beyond sockets goes through [[rustix]]; socket
configuration through [[socket2]]. `unsafe_code` is allowed only in the buffer pool and
forbidden elsewhere.

Release builds use fat LTO, `codegen-units = 1`, `panic = "abort"` (licensed by crash-only
design), `target-cpu` pinned to the deployment box, and PGO via `cargo-pgo` with a recorded
mass-query plus NAR-streaming workload.

## Performance envelope

Stated as targets to measure, not as results:

- **NAR throughput:** a 100 Gbps NIC is 12.5 GB/s. NVMe at 7+ GB/s sequential plus page
  cache hits feed it. Iggy's 5 GB/s on this runtime stack with per-message fsync suggests
  the software is not the limiter. Target: NIC saturation on warm working sets. Caveat: with
  userspace TLS the CPU touches every byte, so the TLS path will fall short of this until
  kTLS proves out.
- **Metadata latency:** the KV read plus page cache is the v1 floor, and harmonia's 82 µs
  from SQLite is the number to beat. In-memory work after the RAM tier lands is tens of ns,
  three orders of magnitude under the ~10–30 µs kernel TCP and syscall path per request.
  **The syscall floor is the floor.** This is the single most important honesty in the
  document: optimizing the metadata plane below the syscall cost changes nothing a client
  can observe, and the only reason to do it is to find out what the ceiling is.
- **What is deliberately not chased:** the last 2× of per-request latency, meaning
  DPDK, `isolcpus`, and SMT-off territory.

## Risk register

| Risk | Severity | Disposition |
|---|---|---|
| io_uring blocked by container seccomp | Deployment | Bare metal or custom profile is a documented hard requirement; Tokio-TPC fallback preserves the architecture |
| io_uring cancellation UAF | Correctness | Pool-owned stable buffers; ring-native timeouts; raw ops module-private |
| Compio per-op boxing | Perf | [[mimalloc]] absorbs it (Iggy-validated); to be re-measured here |
| `send_zerocopy` slower than `send` | Perf | Compio's own docs warn of it; benchmark before adopting |
| Elephant-stream shard skew | Throughput | **Accepted, unmitigated.** eBPF reuseport selection or split accept queues are the named fixes when measured |
| Userspace TLS touches every byte | Perf | Accepted for v1; kTLS spike gated on a measured win |
| zstd-only locks out pre-2.4 clients | Compatibility | Declared client floor; lazy xz transcoding is the escape hatch |
| Missing `Accept-Ranges` silently kills NAR resume | Throughput | Mandatory header on the payload plane, asserted by a conformance test that drops a connection mid-NAR and requires a resumed retry |
| HTTP/1.1-only origin serializes closure queries | Latency | Known and accepted for the skeleton; h2 is the first optimization after it, and nothing durable encodes h1 framing |
| Unbounded growth without GC | Availability | Manual delete only in v1; mechanism proven so policy can land later |
| RAM projection exceeds ~10M paths | Scale | Hard ceiling stated; `redb` remains ground truth beyond it |
| Shard head-of-line blocking | Tail latency | Chunked sends plus yields; heartbeat watchdog |
| ARM ordering bugs | Portability | Acquire/release-only discipline from day one |
| Runtime abandonment (the [[glommio]] lesson) | Strategic | Compio chosen for maintenance velocity; runtime boundary kept thin |

## Alternatives considered

- **Custom in-RAM index as ground truth (v1's design).** Rejected as the *starting* point,
  not as an idea. It requires owning crash consistency, snapshot atomicity, torn-write
  soundness, and an index rebuild on every mutation, all to beat 82 µs that no client can
  perceive. It returns as a derived projection once there is something to profile.
- **[[tokio]] work-stealing.** The correct default for most services, and the contingency
  architecture here. Rejected on the hot path because the serving surface is three URL
  shapes and none of the ecosystem moat applies.
- **[[monoio]].** Better allocation story, narrower io_uring coverage. Iggy's identical
  calculus reached the identical answer.
- **[[glommio]].** Best conceptual fit on paper and the strongest cautionary tale:
  effectively unmaintained with an open memory-corruption report. Its scheduler ideas
  survive here as the watchdog and chunked-send discipline.
- **[[seastar]]/C++.** Proved the model; rejected for implementation language, not
  architecture.
- **A CDN or reverse proxy in front.** This is the standard deployment (cache.nixos.org is
  S3 plus Fastly) and it would make the entire zero-copy payload plane pointless, since the
  origin would serve only cache misses. Rejected because a self-hosted TLS-terminating
  origin is the one stance where the payload plane means anything, and the payload plane is
  the research.
- **`bitcode` for the on-disk record format.** Fast, but its encoding is not stable across
  versions. [[rkyv]] for anything durable.

## What v1 shipped, and where it departs from the above

Written after building it. Everything here is a decision the document did not make, or made
differently, and the reason the code went the other way.

### The write protocol, resolved

The document said `PUT` plus bearer tokens and left the rest implicit. Building it forced
the question of what exactly a client sends, and the answer settles a real ambiguity.

`nix copy --to` uploads the NAR first and the narinfo second, and it compresses
client-side. That collides with `receive → verify → compress → store`: if the client
compressed, the bytes on the wire are not the bytes `NarHash` describes, so the server
cannot verify anything. **v1 therefore requires `?compression=none` on the client's store
URI** and refuses a pre-compressed upload with a message naming the setting. The cache
still stores zstd; it produces that itself.

That constraint turns out to buy the verification story rather than cost it. With no client
compression the upload target is `nar/<nar hash>.nar`, so **the request URL states what its
own body must hash to**. Verification happens during receive, against the request rather
than against a later claim, and a mismatch aborts before anything durable is named. The
narinfo `PUT` then supplies only what the server cannot compute: store path, references,
deriver, `CA`. Every field describing the payload comes from what was received.

**Push credentials arrive as HTTP Basic, not Bearer.** The document said bearer tokens, and
that is what an explicit header carries, but it is not what a Nix client can actually send.
`nix` refuses a client-specified `netrc-file` unless the pushing user is trusted on the
machine, so the only configuration-free way to authenticate a push is credentials in the
store URI, which curl sends as Basic. v1 accepts both schemes and reads the password out of
a Basic credential, ignoring the username: a token already names the node that holds it.
This was found by pushing with a real client, not by reading the source.

**`HEAD` on an uncompressed NAR URL is answered from the NAR-hash index, not the
filesystem.** `BinaryCacheStore::addToStore` probes the URL it is about to upload to, but
bincache stores a recompressed artifact under a different name, so the file is not there
under the name the client asks about. Answering `404` would make every build node re-upload
every NAR forever. This is the one place a `HEAD` does not describe the `GET` of the same
URL, and it is deliberate.

### Deviations from the design as written

- **Atomic appearance is staging plus `rename`, not `O_TMPFILE` plus `linkat`.** Both give
  "appears whole or not at all". `linkat(AT_EMPTY_PATH)` on an `O_TMPFILE` descriptor needs
  privilege on many kernels, and it is Linux-only, which would have made the durability
  primitive untestable off Linux. A crash leaves a staging file rather than nothing; the
  boot sweep collects it, so the recovery story is unchanged.
- **Ingest runs on the shard that accepted the `PUT`, not on dedicated cores.** Compression
  is chunked with a scheduler yield between chunks, the same discipline the payload plane
  uses. Dedicated ingest cores need a cross-thread buffer handoff, and the design's own rule
  is that an unmeasured optimization is a bug in the charter. Uploads are authenticated and
  rare; this is the thing to measure first if push latency ever matters.
- **`arc-swap` is not a dependency.** It was named for publishing the RAM projection, and
  the projection is deferred, so there is nothing to swap. It returns with the projection.
- **`rkyv` runs `unaligned` and `little_endian`.** `redb` hands back a byte slice at
  whatever alignment its page put it, and validating an archive in place needs alignment 1
  to avoid a copy on every read. `little_endian` keeps the on-disk format from changing
  meaning on a different target, which is the same reasoning as the acquire/release rule.
- **`hegel` is not used.** The crate by that name on crates.io is an AWS Lambda payload
  library, not a property-testing framework. Property-shaped tests are written as
  deterministic loops over a seeded generator, so a failure is reproducible from the seed.
- **The index stores records, not rendered bodies.** The document says both, in two places:
  "values are `rkyv`-encoded records, not rendered bytes, so the render format can change
  without a data migration" and, four paragraphs later, "`bincache-index` stores and
  publishes bodies". Records win, because the first sentence's reason is the stronger one
  and because re-signing the world under a new key has to be a pass over fields. Serving
  renders per request; the RAM projection of rendered bodies is the deferred optimization
  that makes the second sentence true.
- **TLS is not implemented.** The document treats userspace `rustls` as the v1 answer and
  defers only kTLS. v1 defers both. The consequence is that the h2-by-default client
  behaviour the document leans on does not arise yet, since `CURL_HTTP_VERSION_2TLS` leaves
  plaintext on HTTP/1.1. Terminating TLS in front is the interim answer and gives up the
  payload plane's reason to exist, which is the tension the document already names.
- **`loom` is not used.** It explores schedules over lock-free structures, and v1 has none:
  the only shared mutable state on the serving path is per-shard relaxed counters, where
  every interleaving is already permitted. It returns with the RAM projection, which is
  where the first real reclamation question appears.
- **`cargo-deny` is not wired up.** Named in the design and still owed, since a service
  holding a signing key does not get to skip dependency auditing.
- **Maintenance is a separate module from the serving-path writer.** A delete needs no
  signing key and no compression level; folding it into the ingest object forced every
  caller to invent values it never used.
- **`redb` allows one writer process**, so `delete`, `reconcile`, and `rotate` need the
  server stopped. Stated in `--help` and in the error.

### What the build found that the document did not predict

- **An empty `References` line must keep its trailing space.** `NarInfo::to_string` writes
  the literal `"References: "` before joining, and `NarInfo::NarInfo` reads every value from
  two characters past the colon. A bare `References:` leaves a client nothing to read. The
  conformance suite caught this on its first run; nothing in the field list suggested it.
- **A heartbeat written in the accept loop reports every idle shard as stalled.** A shard
  parked in `accept` with no traffic is indistinguishable from a wedged one if the loop is
  what updates the timestamp. The heartbeat runs on a timer task instead, which only fails
  to fire when something is monopolizing that shard's executor, which is the condition worth
  reporting. A timestamp also cannot express "never started", since a shard that beat
  immediately reads the same as one that never did, so a beat count sits beside it.
- **`Expect: 100-continue` is not optional.** curl sets it on uploads past about a kilobyte
  and stalls for a second per upload if nothing answers.
- **A successful `PUT` must not close the connection.** The keep-alive decision keyed on
  whether a request announced a body rather than on whether that body had been read, so
  every successful push closed. A push is two requests per path and a closure is hundreds of
  paths, which is a reconnect each.
- **The negative-lookup cache is real and immediately in the way.** During end-to-end
  testing, a client that had once seen a 404 for a path on a given cache URL would not ask
  again, and pointing it at the same server on a different port was the only way to get a
  query out of it. The document predicted this from `nar-info-disk-cache.cc`; it is worth
  recording that it is not a subtle effect. It is also the reason the binary fuse filter
  earns nothing in steady state.

### Verified against a real client

`nix copy --to 'http://user:token@host?compression=none'` pushes, `nix path-info --store
http://host` reads the record back, and `nix copy --from http://host --to file://...` with
`--option trusted-public-keys` set to bincache's key downloads the zstd artifact, verifies
the signature, decompresses, checks `NarHash`, and writes the path into a destination store.
That is the end-to-end proof the testing section asks for. Nix 2.34.8 was the client.

### Deferred, unchanged from the plan above

The RAM projection, the binary fuse filter, huge pages, TLS, HTTP/2, garbage collection,
S3-compatible ingest, `loom`, and kTLS. Each is deferred for the reason given in its own
section, not for lack of time.

## Still open

- **HTTP/2, which is now the top of the queue rather than a vague someday.** Hand-rolled
  HTTP/1.1 with tuned keep-alive still ships first, because it is the shortest path to a
  conformant skeleton. But the client evidence sharpens the stakes: h2 is not something a
  client has to opt into, it is what curl negotiates by default on every TLS connection,
  and `http-connections` caps connections rather than in-flight requests. An HTTP/1.1-only
  origin therefore forces a 500-path closure query into 25 connections' worth of
  serialization, which is precisely the shape issue #5118 measures at 38 s versus 0.73 s.
  This is the highest-value deferred item in the document, it is entirely server-side, and
  the prerender decision above exists to keep it a serving-crate change.
- **kTLS.** Spike it against a 100 MB NAR versus userspace rustls. Adopt only on a measured
  win.
- **S3-compatible ingest.** Named as the next write surface, unscheduled.
- **zstd tuning.** Level policy, trained dictionaries for small-path corpora, recompression
  tiers for cold data.
- **Retention policy.** Mechanism is fixed; TTL versus LRU versus watermark mix, closure
  correctness, and pinning UX all need design when GC is actually built.
- **Credential lifecycle.** Issuance and rotation UX for per-node tokens; multi-key client
  trust windows during signing-key rotation.
- **Packed small-NAR storage.** Measure dentry and file-count pressure first.
