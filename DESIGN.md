---
tags:
  - rust
  - architecture
  - performance
  - concurrency
  - nix
sources:
  - "Raw/Rust/Shard-per-core Rust runtimes - Monoio, Compio, and Glommio compared.md"
  - "Raw/Rust/The blazingly fast Rust crate stack for 2025–2026.md"
  - "Raw/Rust/Type Driven Development in Rust.md"
  - "Raw/Rust/What A+ Rust design actually looks like.md"
  - "Raw/Rust/Modern Rust - the definitive 2023–2026 feature and idiom guide.md"
  - "Raw/Fastest CS/The fastest ways to talk between threads.md"
  - "Raw/Fastest CS/The fastest hash map in computer science, 2025.md"
  - "Raw/Fastest CS/The fastest queue in all of computer science.md"
  - "Raw/Fastest CS/General.md"
last_updated: 2026-07-06
---

# bincache — Design

bincache is a high-performance, read-optimized Nix binary cache: a single stateful service that keeps all per-path metadata in RAM as pre-rendered response buffers and streams ZSTD-compressed NARs from disk with kernel zero-copy. This document takes the high-level bincache specification as its spine and commits it to concrete, benchmarked engineering decisions drawn from the rest of this wiki. Where the spine says "metadata lives in RAM," this document says *which structure, which hasher, which page size, and why*. Every load-bearing number links to the page that defends it.

The one-paragraph thesis: a binary cache is a set of immutable static files served over HTTP, and immutability licenses everything. It licenses [[rcu|zero-read-side-overhead concurrency]], [[perfect-hashing|construction-time index specialization]], permanent response precomputation, [[crash-only-design|crash-only recovery]], and idempotent ingest. The design below is what falls out when you take that license and spend it against the hardware realities documented in [[cache-coherency]], [[mechanical-sympathy]], and [[inter-thread-communication]].

## The Two Workloads

A Nix client substituting a closure produces two request populations with opposite characteristics, and the spine's second principle — split the two workloads — is the organizing decision of this design:

| | Metadata (`.narinfo`, `HEAD`, `nix-cache-info`) | Payload (NAR) |
|---|---|---|
| Size | ~300–800 B | KB to GB |
| Rate | Bursty: a closure resolution fires hundreds of lookups at once (`WantMassQuery`) | Sustained streams |
| Bound by | Per-request fixed cost: syscalls, parsing, lookup | NIC and storage bandwidth |
| Miss rate | High — clients probe for paths the cache may not have | ~Zero — clients only fetch NARs whose narinfo they already resolved |
| Failure sensitivity | Gates every build; latency here is user-visible | Bandwidth here is user-visible |

Notice the asymmetry the spine doesn't spell out: **metadata lookups are miss-dominated, payload fetches are hit-only**. A client resolving a 500-path closure asks about all 500 paths and downloads only the ones we have. The metadata plane must therefore be engineered for fast *negative* answers as much as fast positive ones — this drives the [[binary-fuse-filter]] tier and the choice of miss-optimized probing below.

## System Shape

```
                       ┌────────────────────────────────────────────────┐
                       │  serving shards (one per core, pinned)         │
   NIC (RSS/REUSEPORT) │  ┌──────────┐ ┌──────────┐      ┌──────────┐   │
   ────────────────────┼─▶│ shard 0  │ │ shard 1  │ ...  │ shard N  │   │
                       │  └────┬─────┘ └────┬─────┘      └────┬─────┘   │
                       │       │ lock-free reads (RCU snapshot ptr)     │
                       │  ┌────▼─────────────▼─────────────────▼─────┐  │
                       │  │  shared immutable index                  │  │
                       │  │  hash → &prerendered narinfo response    │  │
                       │  │  (huge-page arena, epoch-reclaimed)      │  │
                       │  └────▲──────────────────────────────────── ┘  │
                       │       │ atomic publish (pointer swap)          │
                       │  ┌────┴─────────────────────────────────────┐  │
   push (authed) ──────┼─▶│  ingest shard: verify → compress → sign  │  │
                       │  └────┬─────────────────────────────────────┘  │
                       │       ▼                                        │
                       │   filesystem: NARs + narinfo blobs + log       │
                       └────────────────────────────────────────────────┘
```

Shared-nothing for connections and mutable state; shared-everything for the immutable index — exactly the spine's fifth principle, implemented as [[thread-per-core]] serving shards over an [[rcu]]-published read-only structure. The ingest pipeline is the only writer and runs on its own dedicated core(s).

## Execution Model: Thread-per-Core on Compio

**Decision: [[compio]] as the runtime, one shard per physical core, [[core-pinning|pinned]], `SO_REUSEPORT` listeners per shard.**

The [[io-uring]] decision framework has three gates, and bincache passes all of them cleanly — which is rare, and worth walking through because it is the justification for taking on io_uring's very real costs:

1. **Does the workload naturally shard?** Better than naturally: the serving path has *no mutable shared state at all*. Every request reads an immutable index and immutable files. There are no hot partitions in the metadata plane because the index is shared read-only across all shards — the [[thread-per-core]] "workload imbalance" failure mode applies to *partitioned* state, and we have none. The 3× overprovisioning penalty from [[thread-per-core|queueing theory]] applies to CPU-bound service time variance; bincache's serving work is I/O-bound and the NIC saturates long before the cores do (see Performance Envelope).
2. **Linux, io_uring available, not in locked-down containers?** bincache is explicitly a stateful pet on one very fast box — the spine trades "someone else's problem" operational simplicity for control. That deployment stance also resolves the [[io-uring|container seccomp blocker]]: run on bare metal or with a custom seccomp profile, and document it as a hard deployment requirement rather than pretending containers are supported.
3. **Can we abandon the Tokio ecosystem?** Yes, because the serving surface is tiny: `GET`/`HEAD` on three URL shapes, HTTP/1.1 with keep-alive. There is no reason to carry [[axum]]/[[hyper]] onto the hot path for that; the request parser is a few hundred lines over owned buffers. This is the same conclusion [[apache-iggy]] reached — and where they needed ecosystem pieces (WebSockets), they paid the port cost knowingly. Our ingest plane has more protocol surface, but ingest is not latency-critical and can afford hand-rolled HTTP too.

Why Compio specifically, per [[shard-per-core-runtimes-compared]]: it is the only actively maintained option ([[glommio]] is effectively unmaintained with a reported SPSC memory-corruption bug from December 2025; [[monoio]]'s maintenance is slowing and its io_uring feature coverage lags), the only one with the splice/buffer-pool/statx coverage the payload path needs, and — decisively — the only one whose decoupled driver-executor enables [[deterministic-simulation-testing]]. [[apache-iggy]] validated exactly this stack at 5 GB/s and sub-millisecond P99 with fsync-per-message persistence, which is a strictly harder durability regime than ours. Compio's one architectural cost, boxing each I/O operation, is negated by [[mimalloc]] — Iggy measured this directly.

**The de-risked fallback, named up front:** if io_uring is unavailable (kernel, seccomp, or an unfixable cancellation bug), the architecture degrades gracefully to the [[tokio|"poor person's thread-per-core"]] — N independent `current_thread` Tokio runtimes with `SO_REUSEPORT`. That keeps every other decision in this document (index design, precomputation, pinning, allocator) and gives up only the io_uring-specific wins: true async file I/O ([[monoio|3.78 µs vs 110 µs for 4 KB random reads]] — a 29× gap) and syscall batching. Expected cost: roughly the difference between 2–3× and 1.5–2× over default Tokio. The design deliberately keeps the runtime boundary thin so this swap stays cheap.

### io_uring hazards, confronted

The [[io-uring]] page documents a cancellation safety crisis: dropping a future while the kernel holds its buffer is use-after-free, and `select!`-style timeouts leak connections on every io_uring runtime. bincache's rules:

- **All I/O buffers are pool-owned, not stack-owned.** Buffers come from a per-shard pool with stable addresses (io_uring registered buffers via Compio's buffer-pool support). A dropped operation returns its buffer to the pool at completion; the kernel never holds a pointer into freed memory even when a future is abandoned.
- **No naked `select!` around in-flight I/O.** Timeouts use ring-native linked-timeout operations, so cancellation is a kernel-visible event, not a silent drop. This is a code-review invariant, enforced by making the raw ops private to one module.
- **Head-of-line blocking:** Compio has no [[glommio]]-style stall detection. The serving path compensates structurally — narinfo responses are single-submission sends (nothing to block on), and NAR streams are chunked sends that re-enter the scheduler between chunks. A per-shard heartbeat timestamp (written [[memory-ordering|relaxed]], read by a watchdog thread) provides poor-man's stall detection with stack-dump-on-SIGUSR1, cribbing Glommio's observability idea for ~50 lines.
- **`RefCell` across `.await`:** the failure mode [[apache-iggy]] hit. Per-shard state is decomposed struct-of-arrays style ([[soa-vs-aos]]) so no borrow spans a yield point; the shared index is immutable so the question doesn't arise there.

### Pinning and topology

Per [[core-pinning]]: pinning is not optional in this model — it *is* the model. Each shard pins to one physical core; NIC IRQ affinity aligns RX queues to serving cores; the ingest core(s) sit on the same NUMA node as the index arena (a [[huge-pages|1 GB huge page]] must come from a single node, so index placement and shard placement must agree — see [[numa-aware-queues]] for the cross-socket cliff). SMT stays **on**: [[core-pinning]] is explicit that disabling SMT is the HFT tail-latency recipe, and for throughput-oriented servers the extra logical threads improve utilization. Likewise no `isolcpus` by default — that buys P99.9 at 4× overprovisioning cost, which is the wrong trade for a bandwidth-bound cache. Both remain documented knobs for a latency-obsessed deployment.

## The Metadata Plane

The spine's requirement: a `.narinfo` request is a hash lookup returning a pointer to a preformatted, immutable response buffer — no parsing, no serialization, no allocation. Here is the machinery.

### Parse, don't validate — at the socket

The request key is a fixed-width 32-character base32 store-path hash. The first thing the server does is [[parse-dont-validate|parse]] it into a `StorePathHash([u8; 20])` [[newtype-pattern|newtype]] — decoded binary, not the string. Malformed requests die at the boundary with a 400 before touching any data structure, and everything downstream operates on a 20-byte fixed key that is correct by construction. This is not just hygiene: fixed-width binary keys are the best case for every structure below, and the decode doubles as free input validation for a public-facing endpoint (the one place [[expert-rust-design|the ponytail rule]] says never to simplify).

### Index structure: frozen base + delta, published via RCU

The access pattern is written-rarely-read-forever — the exact profile where [[frozen-dictionary|.NET's FrozenDictionary]] and [[perfect-hashing]] win, and where [[rcu]] gives readers literally zero synchronization cost. The design is a two-tier structure:

- **Frozen tier:** an immutable, construction-time-optimized table over the full path set, rebuilt in the background every N publishes or M minutes. v1 uses a [[hashbrown]] table frozen at build (built once, never mutated, sized for its exact contents); the named upgrade is a minimal [[perfect-hashing|perfect hash]] (PTHash-class), which eliminates probing entirely — for read-only static data nothing in the [[swiss-table]] family can match it, because a Swiss Table must probe at least once and a perfect hash never does.
- **Delta tier:** a small table holding paths published since the last freeze. Lookups check delta first (it's small and hot), then frozen.
- **Publication:** both tiers hang off a single snapshot pointer, swapped atomically with release ordering and read with acquire — [[memory-ordering|free on x86, one cheap LDAR on ARM]]. Old snapshots are reclaimed via [[crossbeam-epoch]] once all in-flight readers have moved on — the userspace [[rcu]] pattern. Readers never take a lock, never write a shared cache line, never observe a partial update.

Why not just [[papaya]] (lock-free reads, async-safe, purpose-built for read-heavy caches)? It's the right v1 shortcut and the wrong end state: a general concurrent map pays for write-concurrency machinery we don't need, because bincache has *exactly one writer* on a *batch* cadence. The single-writer fact is the same one that makes [[spsc-queue|SPSC queues]] 5–50× faster than MPMC — topology privilege, not cleverness — and snapshot-swap RCU is how you spend it on a map. v1 ships papaya behind the same `Index` trait to get running; the frozen/delta swap is the planned replacement, with the trait boundary making the migration mechanical.

### Hasher

[[foldhash]] (or [[rapidhash]] for the custom frozen tier — it leads the geometric mean at 4.25 ns). The [[fastest-hash-map-2025|deepest insight of the 2025 hash-map landscape]] is that the hasher dominates the table — SipHash→foldhash is a 2–5× swing, bigger than any table choice. The standard counterargument for keeping SipHash is hash-flooding, and it doesn't apply: flooding requires attacker-controlled *insertions*, and index insertions come only from the authenticated push path, keyed by content-derived hashes. Anonymous readers can only send lookup keys, and a miss against a Swiss-table probe (or a perfect hash) is O(1) regardless of key choice. Adversarial-input analysis done; fast hasher licensed.

### Negative lookups: the filter tier

Mass queries are miss-dominated, and at scale the index outgrows cache: 10M paths × ~(20 B key + pointer + table overhead) ≈ hundreds of MB, so a miss costs a DRAM round-trip plus TLB pressure. A [[binary-fuse-filter]] over the key set — rebuilt with each frozen tier, ~9 bits per key, three memory accesses, 13% over the information-theoretic floor — sits in front of the index and answers "definitely absent" from a structure ~25× smaller than the index itself, small enough to stay L3-resident. Definite misses (the common case in a mass-query burst) never touch the index at all.

This is an optimization with a named ceiling, not v1: below ~1M paths the whole index is cache-warm and the filter is pure overhead. Ship without it; add it when the index working set demonstrably exceeds L3.

### Pre-rendered responses in a huge-page arena

At ingest time, the full HTTP response for each narinfo is rendered once — status line, `Content-Type`, `Content-Length`, body with `Sig` line already signed — and stored as one contiguous blob in an append-only arena. The index maps hash → `(ptr, len, nar_file_ref)`. Serving a narinfo hit is: filter check, index lookup, one vectored write of an immutable buffer. `HEAD` responses reuse the same blob's header segment. Nothing is formatted, allocated, or computed per-request; this is the spine's "push work to write time" cashed out to its logical end.

The arena is allocated from [[huge-pages|explicit 2 MB huge pages]] (`MAP_HUGETLB`, THP disabled — the khugepaged promotion stalls and fragmentation decay are exactly why database vendors tell you to turn THP off). Hash-probe-then-blob-read is the random-access pattern that thrashes TLBs; a multi-hundred-MB arena on 4 KB pages means a page-walk per lookup *even when the data is in L1*. 2 MB pages give 32× TLB coverage per entry; the arena plus index on huge pages moves the TLB hit rate on the metadata hot path to ~100%. Per [[huge-pages]], this buys little median latency but removes a whole class of tail outliers — and metadata tail latency is the thing that gates builds.

Arena blobs are freed only by eviction, batched through the same epoch mechanism as index snapshots: unpublish makes a blob unreachable, [[crossbeam-epoch]] defers the free until no reader can hold it. Allocation within a publish batch is bump-style ([[bumpalo]]-class, ~2 ns), because blobs within a batch share a lifetime.

## The Payload Plane

**The page cache is the payload cache, and userspace never sees NAR bytes.** NARs are stored as individual immutable files, content-addressed at their final path (`nar/<filehash>.nar.zst`), served with kernel zero-copy — `sendfile`/`splice`-class ops through io_uring (Compio exposes splice; this coverage was a selection criterion). The serving loop is: `open` (via `statx`-enriched dentry cache), then a loop of bounded zero-copy sends with a scheduler yield between chunks so an elephant stream cannot monopolize its shard.

Deliberate choices against nearby alternatives:

- **No [[direct-io|O_DIRECT]] on the serving path.** Direct I/O is for engines that manage their own caching; the spine explicitly makes the *kernel* the payload cache manager. [[glommio]]'s 7.29 GB/s Optane number is what O_DIRECT buys when you rebuild caching yourself — bincache declines to rebuild it. Where O_DIRECT (or `POSIX_FADV_DONTNEED`) *does* apply: background GC scans and integrity re-verification, which would otherwise evict the hot set — cold maintenance reads must not pollute the cache that serves traffic.
- **TLS via kTLS.** Zero-copy dies if TLS runs in userspace, so TLS termination uses kernel TLS offload: handshake in userspace, symmetric crypto in the kernel, `sendfile` semantics preserved (NIC crypto offload where hardware supports it). *Flag: this is the one load-bearing mechanism this wiki has no page for — the kTLS/io_uring interaction needs a validation spike before it's a resolved decision. The fallback (plaintext behind a TLS-terminating edge/CDN) is acceptable because the spine already treats geography as a deployment concern.*
- **No [[kernel-bypass|DPDK-class kernel bypass]].** The [[kernel-bypass]] page is blunt: full bypass surrenders the kernel networking stack (firewalls, conntrack, TCP) for a ~2× latency win that matters at HFT scale, not here. io_uring is the documented compromise — batched submissions, no syscall per op, kernel features retained. bincache's bottleneck is bandwidth, not per-packet latency.
- **Storage format: plain files, flat directory sharding.** The filesystem is the database. Packed small-NAR archives (many store paths are tiny) are a real future win — less dentry pressure, better locality — deferred until file-count pain is measured, and compatible with the design since the index already stores `(file, offset, len)` refs.

## The Ingest Plane

Ingest is the write path: authenticated, latency-tolerant, and the place where every expensive computation happens exactly once. It runs on dedicated core(s) so compression and signing never steal cycles from serving shards — the workload split made physical.

### Pipeline as a typestate machine

```
receive ──▶ verify ──▶ compress ──▶ store ──▶ render+sign ──▶ publish
 (temp)     (hash)      (zstd)     (linkat)     (arena)      (index)
```

The pipeline is encoded with the [[typestate-pattern]]: `Upload<Receiving> → Upload<Verified> → Upload<Compressed> → Upload<Stored> → Published`, each transition consuming `self`. `publish()` exists only on `Upload<Stored>` whose signature step has run; serving unverified or unsigned content is a compile error, not a code-review catch. This is [[type-driven-development]]'s highest-value application in the codebase: the invariant "partial uploads are never visible" is exactly the kind of structural invariant the pattern makes free. Boundary values (`NarHash`, `Signature`, `Compression`, credentials) are [[parse-dont-validate|parsed newtypes]], per [[nutype]]-style validation at the door.

Mechanically: bytes stream into an `O_TMPFILE` on the target filesystem while the NAR hash is computed incrementally; mismatch against the declared `NarHash` aborts before anything durable exists. ZSTD compression follows (level: default ~9 as the ingest-latency/ratio balance; per-path level tuning and trained dictionaries for small-path dedup stay open). `fsync`, then `linkat` to the final content-addressed name — atomic appearance in the filesystem. The narinfo blob is rendered and signed (pure function — see below), appended to the arena and the publish log, and finally the index insert makes it queryable. Crash at any point leaves either nothing or an orphan file; orphans are collected by GC, and the client's retry is a no-op if the path landed. Idempotency by construction, exactly as the spine demands.

### Functional core, imperative shell

Verification, narinfo rendering, and signing are pure functions — bytes and metadata in, bytes out — per [[functional-core-imperative-shell]]. The shell (sockets, files, index publication) is thin and lives at the edges. This is what makes the pipeline unit-testable without mocks, property-testable with [[proptest]]/[[bolero]] (round-trip: render→parse = id; sign→verify = ok), and — critically — simulatable under [[deterministic-simulation-testing]], since only the shell needs replacing with the mock driver.

### Auth and managed signing

Push credentials are per-node random tokens checked by constant-time compare against an in-memory set — no external round-trip, nothing on the serving path (the auth check literally cannot become a read-path dependency because the read path has no auth). The ed25519 cache key lives only on the server; narinfos are signed at ingest. The security property the spine specifies — a compromised build node can poison only what it uploads, never forge signatures for arbitrary paths — falls out of the key never leaving the box. Key rotation: narinfo blobs are regenerable from stored metadata, so re-signing the world under a new key is a background arena rebuild plus snapshot swap, the same machinery as a freeze. Multi-key trust windows for clients remain open.

### Wire format

**Recommendation: stay HTTP-shaped and per-path.** `HEAD /<hash>.narinfo` is the existence probe (the read path already serves it); `PUT` of NAR then narinfo-metadata mirrors the standard Nix HTTP cache upload shape, meaning `nix copy --to` works against bincache with zero client tooling — an adoption lever worth more than protocol elegance. Content-addressing makes streaming PUTs resumable-by-retry with no session state. A batched binary protocol (closure-at-a-time, [[bitcode]]-encoded manifests — the current wire-format benchmark leader) is the v2 path if per-request HTTP overhead ever shows up in ingest profiles; it will not, because ingest is not the bottleneck by design.

## Memory, Allocation, and Ordering Discipline

- **Global allocator: [[mimalloc]].** The [[rust-memory-allocators|highest-leverage one-line change in Rust]]: up to 5.3× over glibc on small allocations, 13–22% whole-program on allocation-heavy work, ~50% RSS reduction — and it specifically neutralizes Compio's per-op boxing ([[apache-iggy]]'s measured finding). `MIMALLOC_RESERVE_HUGE_OS_PAGES` extends [[huge-pages]] coverage to general heap without code changes. If long-horizon fragmentation bites, [[tikv-jemallocator]] is the named alternative (best latency stability, built-in heap profiling).
- **[[false-sharing|128-byte alignment]] for anything per-shard that a foreign thread reads** — stats counters, heartbeat words, epoch entries. Not 64: the adjacent-cache-line prefetcher makes 64-byte padding insufficient, and the 64→128 correction alone is worth ~1.7× on contended lines. `crossbeam_utils::CachePadded` throughout.
- **Ordering: acquire/release only, written portably from day one.** Snapshot pointer loads are `Acquire`, publishes are `Release` — [[memory-ordering|compiles to plain MOVs on x86 TSO]], correct (with cheap LDAR/STLR) on ARM. This matters because Graviton is a plausible deployment target and TSO-implicit code is exactly what breaks there. `SeqCst` is banned by lint absent a written justification; the [[memory-ordering|seq_cst trap]] is a 20× throughput cliff in hot loops.
- **Cross-core communication: none on the serving path — and keep it that way.** The [[inter-thread-communication]] ladder is a reminder of what every shared atomic costs (35 ns–10 µs depending on how wrong you get it). The only standing cross-core channels are ingest→shards (the snapshot pointer, one release store per publish batch) and shards→watchdog (relaxed heartbeats). If ingest ever needs internal fan-out (parallel compression workers), the tool is one [[rtrb|SPSC ring]] per worker (~7 ns/op, wait-free) — never an MPMC channel where topology allows SPSC, per [[spsc-queue|the topology-privilege rule]].

## Durability and Recovery

The filesystem is the ground truth; everything in RAM is a cache of it. Three artifacts persist:

1. **NAR files** — content-addressed, immutable, fsync'd before publish.
2. **narinfo metadata** — persisted as small per-path records alongside the NARs (also what re-signing regenerates blobs from).
3. **The publish log** — an append-only record of publishes/evictions since the last snapshot.

Boot is: `mmap` the latest **[[rkyv]]-archived index snapshot** (zero-copy access, ~21 ns; validation via rkyv 0.8's safe checked API since the file could be torn), replay the log tail, serve. Snapshotting is a background job that archives the current frozen tier — cheap because the structure is already immutable. A snapshot or log lost to corruption degrades to an O(n) filesystem rescan (slow boot, no data loss); a background fsck reconciles RAM state against disk truth continuously at `IDLE` I/O priority.

This makes bincache **crash-only software**: there is no orderly-shutdown state to lose, `panic = "abort"` is safe (and the [[cargo-profile-optimization|release profile]] uses it), and kill -9 is an acceptable stop mechanism. Immutability is again the enabler — recovery never has to answer "which version," only "present or absent."

Backup story: `rsync`-class replication of the NAR tree plus metadata records *is* a complete backup, because everything else is derivable. The deferred multi-node story starts here too — a warm standby is a replica of the filesystem plus an independent boot.

## Retention and Eviction

Eviction inverts ingest and reuses its machinery: remove from index (snapshot swap), epoch-defer the blob free, then unlink files once no snapshot references them. In-flight NAR streams hold their file's fd, so an unlinked file finishes streaming safely — POSIX semantics doing the reference counting.

Two design constraints the spine implies but doesn't state:

- **Access tracking must not touch the read path.** LRU needs last-access data, but a shared atomic bump per request would put [[cache-coherency|MESI traffic]] on the hottest path in the system. Instead each shard appends access samples to a per-core buffer ([[false-sharing|padded]], relaxed writes, no reader contention); the eviction planner aggregates lazily. Approximate recency is entirely sufficient for cache eviction.
- **Closure integrity.** Evicting a path whose referrers survive produces broken substitutions. The narinfo `References` field gives the dependency edges; the eviction planner treats recently-served paths as GC roots and evicts only complete unreferenced subgraphs, oldest-first. Pinning (release channels, CI baselines) is a root set the operator edits. Full policy (TTL vs LRU vs size watermarks) stays open, but the *mechanism* — batch unpublish through the epoch machinery — is fixed by this design and supports any policy.

## Observability

The spine's open question was what to measure without contaminating the hot path. Answers:

- **[[tracing]]** with `max_level_info` compile-time gating: `debug!`/`trace!` sites cost literally zero in release builds, ~1 ns when compiled in but disabled. Ingest is fully instrumented with spans ([[tracing-error|SpanTrace]] on the error path — errors are [[snafu]]-style structured on the pipeline, where per-stage context is the point); the serving path gets counters, not spans.
- **Per-shard, [[false-sharing|cache-padded]] counters** (requests, hits, misses, bytes, per-bucket latency) written with relaxed stores by the owning shard only, harvested by the metrics endpoint's thread. No shared writes, no contention, no measurable hot-path cost.
- **The watchdog** doubles as the stall detector (heartbeat staleness → stack dump), covering Compio's biggest operability gap versus [[glommio]].

## Testing Strategy

- **[[deterministic-simulation-testing|DST]] as the centerpiece** — the reason Compio's pluggable driver was a selection criterion, not a nicety. Simulated runs inject torn uploads, disk-full mid-pipeline, crash-at-every-await-point during publish, and clock skew, replaying identical schedules on failure. The properties under test are the spine's invariants: no partial path ever visible; retry always safe; boot always converges to disk truth.
- **Property tests** ([[proptest]] / [[bolero]]) on the pure core: narinfo render/parse round-trips, base32 decode, signature verify, filter false-negative-freedom (a filter must *never* say "absent" for a present key).
- **Benchmarks in two registers**, per the wiki's benchmarking split: [[divan]]/[[criterion]] for wall-clock exploration on pinned hardware ([[core-pinning|pin during benchmarks]] or migration noise swamps the signal), and [[iai-callgrind]] for deterministic instruction-count regression gates in CI, where wall-clock is too noisy to gate on.
- **[[cargo-nextest]]** as the runner; [[cargo-deny]] on the supply chain — a service holding a signing key does not get to skip dependency auditing.

## Build and Packaging

Workspace per [[rust-workspace-patterns]] (inherited deps and lints, `unsafe_code` allowed only in the two crates that need it — buffer pool and index — and forbidden elsewhere), shaped by the [[facade-crate-pattern]]:

```
crates/
  bincache-core/     # pure: types, parsing, narinfo render, signing  (no I/O, no async)
  bincache-index/    # snapshot structures, epoch reclamation, arena
  bincache-store/    # filesystem layout, snapshot/log persistence
  bincache-ingest/   # pipeline (typestate), auth
  bincache-serve/    # shards, HTTP, zero-copy send
  bincache/          # facade binary: config, boot, wiring
```

`bincache-core` having no I/O dependency is load-bearing: it is the DST-able, property-testable functional core. Syscall surface beyond sockets goes through [[rustix]] (linux_raw backend — no libc in the way of `linkat`/`statx`/`fadvise`); socket config (`SO_REUSEPORT`, buffer sizes) through [[socket2]].

Release builds per [[cargo-profile-optimization]]: fat LTO, `codegen-units = 1`, `panic = "abort"` (licensed by crash-only design), `target-cpu` pinned to the deployment box (it's a pet — take the SIMD), and **PGO via `cargo-pgo` with a recorded mass-query + NAR-streaming workload** — the 10%+ that most projects never collect, and cheap here because the workload is so easily replayed.

## Performance Envelope, Quantified

The spine's intended bottleneck order — NIC, then RTT, then syscall floor — with this wiki's numbers attached:

- **NAR throughput:** a 100 Gbps NIC is 12.5 GB/s; NVMe (measured 7+ GB/s sequential on [[direct-io|Optane-class hardware]]) plus page-cache hits feed it. [[apache-iggy]]'s 5 GB/s on this same runtime stack — *with* per-message fsync, which we don't pay on reads — says the software stack is not the limiter. Design target: NIC saturation on warm working sets, storage-bandwidth-bound beyond RAM.
- **Metadata latency, in-box:** filter (~3 L3-resident accesses) + index probe + arena read, all TLB-covered by huge pages — the in-memory work is tens of ns, three orders of magnitude under the ~10–30 µs of kernel TCP/syscall path per request. The syscall floor is the floor, exactly as the spine predicts; [[kernel-bypass]] documents what buying past it costs, and we decline.
- **Metadata rate:** bound by request-parse + two syscalls (batched via io_uring, so amortized below two). Mass-query bursts are the design case: misses short-circuit at the filter, hits are single vectored writes of prerendered buffers. There is no per-request allocation, formatting, locking, or shared-cache-line write anywhere in that sentence — which is the whole point.
- **What we deliberately do not chase:** the last 2× of per-request latency (DPDK/`isolcpus`/SMT-off territory) — per [[mechanical-sympathy]], identify the bounding hardware operation and reduce its *count*; the count here is already ~1 of each per request, and the remaining per-op cost is the kernel's, accepted by design.

## Risk Register

| Risk | Severity | Disposition |
|---|---|---|
| io_uring cancellation UAF ([[io-uring]]) | Correctness | Pool-owned stable buffers; ring-native timeouts; raw ops module-private |
| Container seccomp blocks io_uring | Deployment | Bare-metal stance documented; Tokio-TPC fallback preserves architecture |
| Compio per-op boxing | Perf | Neutralized by [[mimalloc]] (Iggy-validated); slab upstreaming possible later |
| Shard head-of-line blocking | Tail latency | Chunked sends + yields; heartbeat watchdog (Compio lacks stall detection) |
| Elephant-stream core skew | Throughput | Accepted: NIC-bound before CPU-bound; [[thread-per-core|3× penalty]] applies to CPU-bound variance |
| kTLS + io_uring interplay unproven | Design gap | Validation spike required; TLS-at-edge fallback is spine-compatible |
| ARM ordering bugs | Portability | Acquire/release-only discipline from day one; [[memory-ordering]] lint |
| Index snapshot corruption | Availability | rkyv checked validation; log replay; O(n) rescan as bottom |
| Hash flooding of index | Security | Insertions authenticated-only → [[foldhash]] licensed; misses O(1) regardless |
| Runtime abandonment (the [[glommio]] lesson) | Strategic | Compio chosen *for* maintenance velocity; runtime boundary kept thin |

## Alternatives Considered

- **[[tokio]], default work-stealing** — the ecosystem moat is real and the correct default *for most services*. Rejected on the hot path because bincache needs none of the moat (no reqwest/axum/tonic on a three-endpoint GET server) and work-stealing's [[cache-coherency|migration cost]] buys rebalancing we don't need against an immutable index.
- **Tokio "poor person's TPC"** — not rejected: it is the contingency architecture, retained deliberately.
- **[[monoio]]** — its slab allocator is genuinely better than boxing, but Linux-only was fine while slower maintenance and io_uring feature gaps were not; Iggy's identical calculus reached the identical answer.
- **[[glommio]]** — the best conceptual fit on paper (DMA storage, latency classes, stall detection) and the strongest cautionary tale: effectively unmaintained, open memory-corruption report, "prepare to fork" as the adoption prerequisite. Its scheduler ideas survive here as the watchdog and chunked-send discipline.
- **[[seastar]]/C++** — proved the whole model; rejected for implementation language, not architecture. This design is, knowingly, Seastar's shape in Rust with a smaller problem.
- **General-purpose DB / external object store for metadata or payloads** — the spine's founding rejection, now quantified: a network round-trip to storage is ~10³–10⁴× the in-RAM lookup it would replace, and [[frozen-dictionary|read-only specialization]] plus the page cache beat generic machinery precisely because they exploit the immutability a general store must not assume.

## Resolved by This Document

Beyond the spine's resolved set (push ingest, ZSTD, managed signing): runtime = [[compio]] thread-per-core with Tokio-TPC fallback; index = RCU-swapped frozen+delta over a huge-page arena (papaya as v1 stand-in), [[foldhash]]/[[rapidhash]] hashing; responses fully prerendered at ingest; payloads = plain files + page cache + kernel zero-copy, no O_DIRECT on serve; ingest = typestate pipeline over a pure core; durability = rkyv snapshot + publish log over filesystem ground truth, crash-only; allocator = [[mimalloc]]; wire format = HTTP-shaped per-path push for `nix copy` compatibility; testing = DST-first.

## Still Open

- **kTLS validation** — the single unverified load-bearing mechanism (spike before committing).
- **ZSTD tuning** — level policy, trained dictionaries for small-path corpora, re-compression tiers for cold data.
- **Retention policy** — mechanism fixed (epoch unpublish, closure-aware planner); policy (TTL/LRU/watermark mix, pinning UX) needs operator input.
- **Credential lifecycle** — issuance/rotation UX for per-node tokens; multi-key client trust during signing-key rotation.
- **Packed small-NAR storage** — measure dentry/file-count pressure first.
- **v2 batched push protocol** — [[bitcode]]-encoded closure manifests, only if ingest profiles ever demand it.
