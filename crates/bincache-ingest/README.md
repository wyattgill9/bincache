# bincache-ingest

The write path: authenticated, latency-tolerant, and the only writer to the index. Every
expensive computation in the system happens here, exactly once per path.

## Owns

- **The pipeline, as a typestate machine.** `Upload<Receiving>` to `Upload<Verified>` to
  `Upload<Compressed>` to `Upload<Stored>` to `Published`, each transition consuming
  `self`. `publish()` exists only on a state whose verify and sign steps have run, so
  serving unverified or unsigned content is a compile error rather than a review catch.
- **The stages.** Incremental NAR hashing during receive with an abort on mismatch against
  the declared hash, ZSTD compression (level policy still open), the call into
  `bincache-store` for durable placement, the call into `bincache-core` for render and
  sign, then the arena append and index publish.
- **Push auth.** Per-node random tokens checked by constant-time compare against an
  in-memory set. No external round-trip. The read path has no auth check to become a
  dependency of.
- **The signing key at runtime.** The ed25519 key never leaves the box, which is what
  bounds a compromised build node to poisoning only what it uploads. Rotation is a
  background re-render of blobs from stored metadata records plus a snapshot swap, reusing
  the freeze machinery.
- **The push wire format.** HTTP-shaped and per-path (`PUT` of NAR then narinfo metadata,
  `HEAD` as the existence probe) so `nix copy --to` works unmodified.

## Does not own

The serving path, and nothing here may sit on it. Compression and signing run on dedicated
cores so they never steal cycles from serving shards.

## Depends on

`bincache-core`, `bincache-index`, `bincache-store`.

## Open

Retention and eviction is the second writer against the index and has no home yet.
Mechanism is fixed (batch unpublish through the epoch machinery, closure-aware planner
using narinfo `References`, per-shard access samples aggregated lazily); policy is not.
When policy lands it is a candidate for its own crate rather than a second concept inside
this one.

## Design references

DESIGN.md: "The Ingest Plane", "Auth and managed signing", "Wire format", "Retention and
Eviction".
