# bincache-ingest

The write path: authenticated, latency-tolerant, and the only writer to the index. Every
expensive computation in the system happens here, exactly once per path.

## Owns

- **The upload machine, as a typestate.** `Upload<Receiving>` to `Upload<Compressed>` to
  `Upload<Verified>`, each transition consuming `self`. `store` exists only on the verified
  state, so committing unverified content is a compile error rather than a review catch.
  The hash to verify against comes from the request URL, so the target states what its own
  body must hash to and a mismatch aborts before anything durable is named.
- **The publish.** Parse the client's narinfo, find the NAR it describes, take every
  payload-describing field from what was actually received, sign, commit.
- **Managed signing.** The key never leaves this process and the client's own `Sig` lines
  are discarded, which is what bounds a compromised build node to poisoning only what it
  uploads. Rotation is a background pass over the records.
- **Push auth.** Per-node bearer tokens compared as fixed-width digests with no early exit,
  so neither a token's length nor its first differing byte is observable in timing. The
  read path has no auth check to become a dependency of.
- **Maintenance.** Delete, reconcile, and rotate live in their own module: a delete needs no
  signing key and no compression level, and folding it in would force callers to invent
  values they never use.

## Does not own

The serving path, and nothing here may sit on it.

## Depends on

`bincache-core`, `bincache-index`, `bincache-store`.

## Notes

Clients must push with `?compression=none`. bincache verifies the NAR hash the protocol
defines, over the bytes the protocol defines, and produces the zstd artifact itself. A
pre-compressed upload is refused with a message naming the setting.

Compression currently runs on the shard that accepted the upload, chunked with a yield
between chunks. Dedicated ingest cores are the named next step and want a measurement
first.

## Open

Retention policy. The mechanism is proven by operator-triggered delete; TTL versus LRU
versus watermark, closure correctness, and pinning UX all need design when GC is built.

## Design references

DESIGN_V2.md: "The ingest plane", "Wire format and auth", "Managed signing".
