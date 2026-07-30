# bincache-serve

The read path: thread-per-core serving shards over an immutable index. No mutable shared
state, no auth, no allocation per request.

## Owns

- **The execution model.** One shard per physical core on a Compio runtime, pinned, with a
  `SO_REUSEPORT` listener each. The runtime boundary stays thin so the documented fallback
  (N independent `current_thread` Tokio runtimes with the same sharding) is a cheap swap.
- **The HTTP surface.** Hand-rolled HTTP/1.1 with keep-alive over pool-owned buffers,
  covering `GET` and `HEAD` on three URL shapes: `nix-cache-info`, `<hash>.narinfo`, and
  `nar/<filehash>.nar.zst`. The request key is decoded into a `StorePathHash` at the
  socket; malformed requests get a 400 before touching any data structure.
- **The metadata response.** Snapshot load, lookup, one vectored write of the immutable
  pre-rendered blob. `HEAD` reuses that blob's header segment. Nothing is formatted,
  allocated, or computed per request.
- **The payload response.** Bounded zero-copy sends (splice-class through io_uring) from
  the descriptor `bincache-store` opened, with a scheduler yield between chunks so one
  elephant stream cannot monopolize its shard. Userspace never sees NAR bytes.
- **The io_uring safety rules.** All I/O buffers come from a per-shard pool with stable
  registered addresses, so an abandoned operation never leaves the kernel holding freed
  memory. Timeouts are ring-native linked operations, never a `select!` around in-flight
  I/O. The raw ops stay private to one module so this is enforceable by review.
- **Hot-path observability.** Per-shard 128-byte-aligned counters written with relaxed
  stores by the owning shard only and harvested elsewhere, plus a relaxed heartbeat
  timestamp that gives the watchdog stall detection Compio does not provide. Counters, not
  spans.

## Does not own

Any write to the index or the filesystem. It holds a snapshot pointer and file
descriptors, nothing more.

## Depends on

`bincache-core`, `bincache-index`, `bincache-store`.

## Notes

The buffer pool is the second place licensed to carve out `#![allow(unsafe_code)]`. Socket
configuration goes through `socket2`.

## Design references

DESIGN.md: "Execution Model: Thread-per-Core on Compio", "io_uring hazards, confronted",
"Pinning and topology", "The Payload Plane", "Observability".
