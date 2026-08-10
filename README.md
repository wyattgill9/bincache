# bincache

A Nix binary cache you run yourself. Build nodes push store paths to it, and other machines
substitute from it.

It speaks the binary cache protocol `nix copy` and `substituters` already use, so clients
need a URL and a public key.

`ARCHITECTURE.md` is the map.

## Limits

- No TLS. Terminate it in front, or stay on a trusted network.
- No garbage collection. The cache grows until you delete something.
- Clients need Nix 2.4 or newer, because every artifact is stored as zstd.

## Set it up

`nix build` produces the same binary at `result/bin/bincache` if you would rather not build
from a checkout.

```sh
cargo build --release
cp target/release/bincache /usr/local/bin/bincache
mkdir -p /var/lib/bincache

# secret key goes to stdout, public key line to stderr. Save that line: clients need it.
bincache keygen --name cache.example.org-1 > /var/lib/bincache/secret.key

# one credential per build node that uploads
bincache token >> /var/lib/bincache/push.token

bincache serve \
    --data-dir /var/lib/bincache \
    --secret-key-file /var/lib/bincache/secret.key \
    --push-token-file /var/lib/bincache/push.token \
    --listen 0.0.0.0:5000
```

No push credential = a read-only replica. `bincache serve --help` is the full flag list, and
every flag also takes an env var. The ones worth knowing: `--zstd-level` (3) and
`--priority` (30, where lower wins and `cache.nixos.org` is 40).

## Push to it

```sh
nix copy --to 'http://bincache:<token>@cache.example.org:5000?compression=none' /nix/store/...
```

`?compression=none` is required. bincache hashes the uncompressed NAR the protocol defines
and produces the zstd artifact itself.

The username is ignored, only the token matters. netrc works too and keeps the token out of
the process table:

```
machine cache.example.org login bincache password <token>
```

A refused push answers `400` with the reason in the body, which `nix copy` prints under
`response body:`. Causes: `?compression=none` missing, bytes corrupted in transit, a narinfo
published before its NAR, or an empty NAR. `401` means a bad or missing token. `500` means
the cache failed, the reason is in the server log, and retrying is reasonable.

## Read from it

In `nix.conf`, using the line `keygen` printed to stderr:

```
substituters = http://cache.example.org:5000
trusted-public-keys = cache.example.org-1:<base64>
```

Nix checks the signature and `NarHash` before writing anything into the store, and refuses a
path bincache did not sign. Interrupted downloads resume (`Accept-Ranges: bytes`).

Two things that will otherwise cost you an afternoon:

- **A non-trusted user cannot add substituters.** The daemon ignores `--substituters` and
  `--trusted-public-keys` on the command line and says nothing about it. Put the cache in
  `nix.conf` and restart the daemon, or test against a standalone `--store /some/path`.
- **Clients cache misses for an hour** (`narinfo-cache-negative-ttl`). A path you just
  pushed still reads as absent, so a working cache looks broken. `--refresh` bypasses it.

## Operate it

`GET /metrics` serves Prometheus text: request counts, metadata hits and misses, bytes
served, uploads, rejections, and paths held. `bincache_paths` is counted at boot and
tracked per publish, so a `delete` while the server runs is not reflected until it
restarts.

These run against a live server. There is no lock file and no single-writer database, and
every write is an atomic rename:

```sh
bincache reconcile --data-dir /var/lib/bincache   # payload tree against records, both ways
bincache delete    --data-dir /var/lib/bincache <32-char store path hash>
bincache rotate    --data-dir /var/lib/bincache --secret-key-file <new key>
```

`delete` forgets the record and leaves the artifact. A NAR does not include the store path
name, so two paths with identical contents share one artifact, and unlinking it would
strand the other. `reconcile` reports what is left unreferenced.

`rotate` replaces the signature on every record rather than adding one, so the old key
verifies nothing afterwards. Order matters:

1. Add the new public key to every client's `trusted-public-keys`.
2. Rotate.
3. Keep the old key listed until every client has refetched. A client holds an old signature
   for `narinfo-cache-positive-ttl` (30 days by default) and the server cannot invalidate
   that. `nix copy --refresh` bypasses it per path.

## The protocol surface

| Route | Returns |
|---|---|
| `GET /nix-cache-info` | `StoreDir`, `WantMassQuery`, `Priority` |
| `GET`, `HEAD /<hash>.narinfo` | the signed manifest for one store path, or 404 |
| `GET /nar/<file hash>.nar.zst` | the compressed NAR, resumable |

`PUT` on the same shapes is the push side. The signing key never leaves the server: client
`Sig` lines are discarded and every record is re-signed locally, so a compromised build node
can poison only the paths it uploads, and only until you delete them.

## Contributing

`direnv allow` (or `nix develop`) puts the pinned toolchain, `cargo-nextest`, and the Python
the scripts below import into your shell. `rust-toolchain.toml` is the pin, so rustup users
get the same compiler without Nix.

```sh
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

`nix flake check` runs the clippy and nextest gates plus formatting in the sandbox, which is
what CI does.

Three layers of test, in increasing strength:

- `crates/bincache-serve/tests/conformance.rs` runs a real server on a real socket and
  asserts the exact bytes a Nix client depends on. Part of `cargo nextest run`.
- `scripts/conformance.py` asserts the same against a running process, and verifies the
  served signature over the canonical fingerprint the way a client does.
- `scripts/e2e-nix.py` is the one that proves it works. It pushes a freshly built path,
  substitutes it into a separate store with a real Nix client, then repeats that with a key
  the cache did not sign with and requires the client to refuse.

```sh
cargo build --release && python3 scripts/e2e-nix.py
```

The e2e destination has to be a store (`--to /some/path`), not a binary cache
(`--to file://...`). A binary cache destination re-uploads without checking signatures, so a
test using one passes even when the signature is wrong.

## Layout

```
crates/
  bincache-core/     types, base32, narinfo render and parse, fingerprint, signing
  bincache-store/    the data directory: artifacts, published records, receipts
  bincache-ingest/   upload state machine, publish, auth, maintenance
  bincache-serve/    axum router, routing, ranges, counters
  bincache/          config, boot, wiring
```

On disk:

```
<data-dir>/
  narinfo/<store path hash>.narinfo   the exact bytes GET returns
  bynar/<nar hash>                    "<file hash> <file size> <nar size>"
  nar/<ab>/<file hash>.nar.zst        the compressed artifact
  staging/                            uploads that have not committed
```
