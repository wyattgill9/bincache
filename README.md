# bincache

A Nix binary cache origin you run yourself. Build nodes push store paths to it, and other
machines substitute from it instead of rebuilding.

It speaks the binary cache protocol that `nix copy` and `substituters` already use, so
clients need a URL and a public key and no patching.

It is also a performance-research vehicle for the question of where the ceiling sits on
this workload given thread-per-core, io_uring, and content-addressed storage.
`research/DESIGN_V2.md` is the argument and `ARCHITECTURE.md` is the map. Neither is
required reading to run the thing.

## Before you start

**io_uring has to be available.** Docker 25.0.0+ and containerd's `RuntimeDefault` seccomp
profile block it, and Kubernetes inherits that. Run bincache on bare metal, or in a
container whose seccomp profile permits `io_uring_setup`, `io_uring_enter`, and
`io_uring_register`. Default-profile containers are unsupported. On a platform with no
io_uring at all, Compio falls back to a polling driver and everything still works, without
the io_uring wins.

**There is no TLS.** Terminate it in front, or keep the cache on a trusted network.

**There is no garbage collection.** The cache grows until you delete something.

Clients need Nix 2.4 or newer, because every artifact is stored as zstd.

## Set it up

```sh
cargo build --release
cp target/release/bincache /usr/local/bin/bincache
mkdir -p /var/lib/bincache
```

Generate a signing key. The secret goes to stdout and the public key to stderr, so redirect
them separately and keep the line stderr prints: every client that reads from this cache
needs it.

```sh
bincache keygen --name cache.example.org-1 > /var/lib/bincache/secret.key
# stderr: trusted-public-keys entry: cache.example.org-1:<base64>
```

Generate a push credential for each build node that will upload:

```sh
bincache token >> /var/lib/bincache/push.token
```

Then serve:

```sh
bincache serve \
    --data-dir /var/lib/bincache \
    --secret-key-file /var/lib/bincache/secret.key \
    --push-token-file /var/lib/bincache/push.token \
    --listen 0.0.0.0:5000
```

With no push credential configured the cache is read-only, which is a reasonable way to run
a replica. Every setting also takes an environment variable, and `bincache serve --help`
is the full list. The ones worth knowing:

| Setting | Default | What it changes |
|---|---|---|
| `--listen` | `0.0.0.0:5000` | every shard binds it with `SO_REUSEPORT` |
| `--shards` | reported parallelism | independent accept loops |
| `--pin` | off | pin each shard to a core, for a dedicated box |
| `--zstd-level` | `3` | compression applied once, at ingest |
| `--priority` | `30` | lower wins when several caches hold a path; `cache.nixos.org` is 40 |
| `--want-mass-query` | `true` | whether `nix-cache-info` invites the bulk narinfo queries closure resolution produces |

## Push to it

```sh
nix copy --to 'http://bincache:<token>@cache.example.org:5000?compression=none' /nix/store/...
```

`?compression=none` is required. bincache verifies the NAR hash over the bytes the protocol
defines and produces the zstd artifact itself, so it needs the uncompressed stream. Leave
the setting off and the push is refused, with the reason in the response body.

The username in the URI is ignored; only the token matters. The credential can also go in
netrc or an `Authorization: Bearer` header. Prefer netrc when the pushing user is trusted on
that machine, since it keeps the token out of the process table:

```
machine cache.example.org login bincache password <token>
```

Nix refuses a client-specified `netrc-file` for an untrusted user, which is why the URI form
exists.

A push is two requests per path: the NAR, then the narinfo that publishes it. Both travel on
one connection, so pushing a whole closure is one handshake.

### When a push is refused

bincache answers `400` and says why in the body, and `nix copy` prints that under
`response body:`. The ones you are likely to see:

| Message | Cause |
|---|---|
| `...Point the client at the cache with ?compression=none` | the setting is missing from the store URI |
| `uploaded bytes hash to X but the request target declares Y` | the upload was corrupted in transit |
| `no NAR with hash X has been uploaded` | a narinfo was published before its NAR |
| `an uploaded NAR must not be empty` | Nix rejects a zero `NarSize`, so bincache will not store one |

A `401` means the token was missing or wrong. A `500` means the cache itself failed, and the
reason is in the server log rather than the response; retrying is reasonable.

## Read from it

In `nix.conf`, using the line `keygen` printed to stderr:

```
substituters = http://cache.example.org:5000
trusted-public-keys = cache.example.org-1:<base64>
```

Nix then substitutes from bincache the same way it substitutes from `cache.nixos.org`: it
fetches the narinfo, checks the signature against `trusted-public-keys`, downloads the zstd
NAR, and verifies `NarHash` before writing anything into the store. A path bincache did not
sign is refused.

Two things that will otherwise cost you an afternoon:

- **A non-trusted user cannot add substituters.** If your account is not in `trusted-users`,
  the daemon ignores `--substituters` and `--trusted-public-keys` on the command line and
  says nothing about it. Put the cache in `nix.conf` and restart the daemon, or test against
  a standalone store with `--store /some/path`, which bypasses the daemon.
- **A client caches misses for an hour** (`narinfo-cache-negative-ttl`, 3600 seconds). Push
  a path the client has already asked for and it keeps reporting the path as absent, so a
  cache that is working correctly looks broken. `--refresh` bypasses the cached miss, and
  so does testing with a path that client has never asked for.

Interrupted downloads resume: NAR responses advertise `Accept-Ranges: bytes` and serve
`206` for a range request, so a dropped transfer picks up where it stopped rather than
restarting.

## Operate it

`GET /metrics` serves Prometheus text: request counts, metadata hit and miss counts, bytes
served, uploads, rejections, and the number of paths held.

Three subcommands need the server stopped, because `redb` allows one writer process at a
time:

```sh
bincache reconcile --data-dir /var/lib/bincache   # payload tree against index, both ways
bincache delete    --data-dir /var/lib/bincache <32-char store path hash>
bincache rotate    --data-dir /var/lib/bincache --secret-key-file <new key>
```

`rotate` replaces the signature on every record rather than adding one, so the old public
key verifies nothing the server serves afterwards. Two things follow, and getting them
backwards is how a rotation breaks a build farm:

- Add the new public key to every client's `trusted-public-keys` before you rotate. Keep
  the old one listed until every client has refetched.
- A client that already fetched a narinfo keeps the old signature for
  `narinfo-cache-positive-ttl`, which defaults to 30 days. Until it expires, that client
  verifies against the old key and fails against the new one alone. `nix copy --refresh`
  bypasses the cache for a given path; there is no way to invalidate it from the server.

## The protocol surface

Three routes, which is all of it. `PUT` on the same shapes is the push side.

| Route | Returns |
|---|---|
| `GET /nix-cache-info` | `StoreDir`, `WantMassQuery`, `Priority` |
| `GET`, `HEAD /<hash>.narinfo` | the signed manifest for one store path, or 404 |
| `GET /nar/<file hash>.nar.zst` | the compressed NAR, resumable |

The signing key never leaves the server. Client `Sig` lines in a pushed narinfo are
discarded and the record is re-signed locally, so a compromised build node can poison only
the paths it uploads, and only until you delete them.

## Contributing

```sh
cargo nextest run --workspace       # includes conformance against a real socket
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

Three layers of test, in increasing strength.

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

`scripts/e2e-nix.py` is the one that proves it works. It starts a fresh cache, pushes a
freshly built path with `nix copy --to`, reads the record back with `nix path-info`, and
substitutes it into a separate store, which makes a real client verify the signature,
decompress, and check `NarHash` before writing anything. It then repeats that last step with
a key the cache did not sign with and requires the client to refuse.

```sh
cargo build --release && python3 scripts/e2e-nix.py
```

The destination has to be a store (`--to /some/path`), not a binary cache
(`--to file://...`). A binary cache destination re-uploads without checking signatures, so a
test using one passes even when the signature is wrong.

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
