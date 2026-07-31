# bincache

A Nix binary cache origin, built to find where the ceiling sits for this workload when you
spend thread-per-core, io_uring, and content-addressed storage on it.

It is a performance-research vehicle, and that framing carries two obligations: it has to
be correct against real clients before anything else, and every optimization has to arrive
with a before/after number. `research/DESIGN_V2.md` is the argument; `ARCHITECTURE.md` is
the map.

## What it does

Three routes, which is the whole protocol:

| Route | Returns |
|---|---|
| `GET /nix-cache-info` | `StoreDir`, `WantMassQuery`, `Priority` |
| `GET`, `HEAD /<hash>.narinfo` | the signed manifest for one store path, or 404 |
| `GET /nar/<file hash>.nar.zst` | the compressed NAR, resumable |

Pushes arrive as `PUT` on the same shapes, authenticated with a per-node token. The server
verifies the NAR hash against the request target, compresses to zstd itself, and signs with
a key that never leaves the box.

## Running it

```sh
cargo build --release

# The secret goes to stdout; the trusted-public-keys entry goes to stderr.
./target/release/bincache keygen --name cache.example.org-1 > /var/lib/bincache/secret.key

./target/release/bincache token > /var/lib/bincache/push.token

./target/release/bincache serve \
    --data-dir /var/lib/bincache \
    --secret-key-file /var/lib/bincache/secret.key \
    --push-token-file /var/lib/bincache/push.token \
    --listen 0.0.0.0:5000
```

`bincache serve --help` lists every setting and its environment variable.

### Pushing to it

```sh
nix copy --to 'http://bincache:<the token>@cache.example.org:5000?compression=none' /nix/store/...
```

`compression=none` is required. bincache verifies the NAR hash over the bytes the protocol
defines and produces the zstd artifact itself, so it needs the uncompressed stream. A
pre-compressed upload is refused with a message naming the setting.

The credential can go in the URI as above, in netrc, or in an `Authorization: Bearer`
header. The username is ignored. Prefer netrc when the pushing user is trusted on that
machine, since it keeps the token out of the process table:

```
machine cache.example.org login bincache password <the token>
```

Nix refuses a client-specified `netrc-file` for an untrusted user, which is why the URI form
exists.

### Reading from it

```
substituters = http://cache.example.org:5000
trusted-public-keys = cache.example.org-1:<the base64 from keygen>
```

Clients need Nix 2.4 or newer, because everything is stored as zstd.

### Operating it

`GET /metrics` serves Prometheus text. `bincache reconcile` compares the payload directory
against the index in both directions, `bincache delete` removes one path, and
`bincache rotate` re-signs every record under a new key. Those three need the server
stopped, because `redb` allows one writer process at a time.

There is no garbage collection. The cache grows until an operator deletes something.

## Deployment requirements

io_uring is blocked by default in Docker 25.0.0+ and in containerd's `RuntimeDefault`
seccomp profile, which Kubernetes inherits. **bincache runs on bare metal, or in a
container with a seccomp profile that permits `io_uring_setup`, `io_uring_enter`, and
`io_uring_register`.** Default-profile containers are unsupported. On platforms without
io_uring, Compio falls back to a polling driver and everything still works, minus the
io_uring-specific wins.

TLS is not implemented. Terminate it in front, or wait for the in-process `rustls` work.

## Testing

```sh
cargo nextest run --workspace     # includes conformance against a real socket
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

Three layers, in increasing strength.

`crates/bincache-serve/tests/conformance.rs` runs a real shard on a real socket and asserts
the exact bytes a Nix client depends on. It is part of `cargo nextest run`.

`scripts/conformance.py` asserts the same against a running process, and additionally
verifies the served signature the way a client does, over the canonical fingerprint:

```sh
python3 scripts/conformance.py \
    --base-url http://127.0.0.1:5000 \
    --token "$(cat push.token)" \
    --public-key 'cache.example.org-1:<base64>'
```

`scripts/e2e-nix.py` is the one that actually proves it. It starts a fresh cache, pushes a
freshly built path with `nix copy --to`, reads the record back with `nix path-info`, and
substitutes it into a separate store with `nix copy --from`, which makes the client verify
the signature, decompress, and check `NarHash` before writing anything. It then repeats that
last step with a key the cache did not sign with and requires the client to refuse.

```sh
cargo build --release && python3 scripts/e2e-nix.py
```

Two things that will otherwise waste an afternoon, both learned the hard way here. The
destination has to be a store (`--to /some/path`), not a binary cache (`--to file://...`):
a binary cache destination re-uploads without checking signatures, so a test using one
passes even when the signature is wrong. And a client caches negative narinfo lookups for
an hour with a floor `--refresh` cannot lower, so testing a path the client has ever missed
on reports a failure that is not one; the script builds a unique path per run to avoid it.

## Layout

```
crates/
  bincache-core/     pure: types, base32, narinfo render and parse, fingerprint, signing
  bincache-index/    redb schema, rkyv records
  bincache-store/    content-addressed NAR files, atomic placement, orphan scan
  bincache-ingest/   typestate upload machine, publish, auth, maintenance
  bincache-serve/    shards, HTTP/1.1, routing, ranges, counters
  bincache/          config, boot, wiring
```
