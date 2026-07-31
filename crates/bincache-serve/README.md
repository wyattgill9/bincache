# bincache-serve

The read path: thread-per-core serving shards over an immutable index. No mutable shared
state, no auth on reads, no lock anywhere on the serving path.

## Owns

- **The execution model.** One shard per core on a Compio runtime, optionally pinned, each
  with its own `SO_REUSEPORT` listener. The runtime boundary stays thin so the documented
  fallback (N independent `current_thread` Tokio runtimes with the same sharding) is a cheap
  swap.
- **The HTTP surface.** Hand-rolled HTTP/1.1 with keep-alive, covering `GET`, `HEAD`, and
  `PUT` on `nix-cache-info`, `<hash>.narinfo`, and `nar/<file hash>.nar<ext>`, plus
  `/metrics`. Framing is built per request and nothing durable encodes it, which is what
  keeps HTTP/2 a change to this crate.
- **Routing.** The request key is decoded into a typed value at the socket; malformed input
  gets a 400 before touching any data structure.
- **The payload response.** Bounded reads and writes through one reused buffer, with the
  shard handed back to its scheduler between chunks so one elephant stream cannot
  monopolize it. `Accept-Ranges: bytes` is mandatory and `Content-Encoding` is never set:
  a client resumes a dropped NAR only if the first response advertised the one and omitted
  the other.
- **Hot-path observability.** Per-shard 128-byte-aligned counters written with relaxed
  stores by the owning shard only and harvested elsewhere, plus a heartbeat that gives the
  watchdog the stall detection Compio does not provide. Counters, not spans.

## Does not own

Any write to the index or the filesystem. It holds handles and file descriptors.

## Depends on

`bincache-core`, `bincache-index`, `bincache-ingest`, `bincache-store`.

## Notes

`tests/conformance.rs` runs a real shard on a real socket and asserts the exact bytes a Nix
client depends on. Each check names the client behaviour it protects.

Buffers are per-connection and reused across requests on that connection. The design's
registered per-shard pool with stable addresses is the named upgrade, unbuilt because it
would be an unmeasured optimization.

## Design references

DESIGN_V2.md: "Execution model: thread-per-core on Compio", "Accept model, and the skew it
causes", "The payload plane", "Observability", "Testing".
