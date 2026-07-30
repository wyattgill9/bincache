# How Attic (the Nix Binary Cache) Works: A Technical Deep-Dive

## TL;DR
- **Attic is a self-hostable, multi-tenant Nix binary cache server** (Rust; `atticd` server + `attic` client + `atticadm` admin tool) backed by SQLite/PostgreSQL for metadata and local-filesystem or S3-compatible object storage for data. Its distinguishing features are **global content-defined-chunking deduplication (FastCDC)**, **server-managed on-the-fly signing**, and **JWT-based multi-tenant access control** — solving the "every machine needs the signing key and there's no recency-based garbage collection" problem of plain `s3://`/`ssh://` stores while being self-hostable, unlike Cachix.
- **The data model splits every cache into restricted views over a global content-addressed NAR store and chunk store**: NARs are split into chunks, identical chunks are stored once, and garbage collection cascades in three levels (local mapping → orphan NARs → orphan chunks). Downloads reassemble NARs server-side by streaming constituent chunks; signing happens at fetch time so pushers never touch the signing key.
- **As of mid-2026 the upstream `zhaofengli/attic` repo is effectively stalled** ("7 months of no activity" per the Celler fork announcement, still self-described as an "early prototype," no tagged releases). This spawned an actively developed drop-in fork, **Celler** (Cyberus Technology, April 27, 2026), and prompted migration to alternatives like **ncps** and **niks3**. Attic still works and is widely deployed, but new production deployments should weigh the maintenance situation.

## Key Findings
- Attic is written mostly in Rust (~89% of the repo) and licensed Apache-2.0. It was announced by Zhaofeng Li on the NixOS Discourse on **January 1, 2023** ("Hi Nix community, I would like to show you something that I've been working on this holiday…"), and is still labeled "an early prototype" in its README and docs as of 2026.
- The server (`atticd`) runs in modes selected by `--mode`: `monolithic` (default, all components), `api-server` (stateless, replicable), `garbage-collector` (periodic, not replicable), `garbage-collector-once`, `db-migrations`, and `check-config`.
- Metadata lives in a relational database (SQLite by default, PostgreSQL for production); the actual NAR/chunk bytes live in a storage backend (`local` filesystem path or `s3`-compatible object storage). Both are configured in `server.toml`.
- Deduplication is global and two-level (whole-NAR and sub-NAR chunks via FastCDC). Individual caches are just access-controlled "views" onto this shared content-addressed store, so the same store path pushed to ten caches is stored once.
- Authentication is stateless: signed JWTs carry the permission set. Tokens are signed with either an HS256 symmetric secret or an RS256 keypair, supplied base64-encoded via environment variables. Permissions are per-cache-name-pattern (wildcards allowed): pull, push, delete, create-cache, configure-cache, configure-cache-retention, destroy-cache.
- Signing is server-managed: each cache has its own Nix signing keypair, the server signs narinfos on the fly at fetch time, and clients trust the cache via its public key (added to `trusted-public-keys` automatically by `attic use`).
- The chunking parameters and their gotchas (changing them wrecks the dedup ratio; small default chunk sizes hurt performance on S3) have become the single most important operational tuning knob.

## Details

### 1. What Attic is and the problem it solves
**Attic** is a self-hostable Nix Binary Cache server backed by an S3-compatible storage provider, with support for global deduplication and garbage collection. It is "multi-tenant": one server hosts many named caches that are mutually untrusting, so you can run a private cache for yourself, one for your team, and public caches for the world, all from one deployment.

In the launch announcement Zhaofeng Li laid out what was wrong with the pre-existing options, and this framing is still the clearest statement of Attic's rationale:
- **Plain `s3://` store** (`nix copy --to s3://…`): every machine touching the cache needs its own S3 access key; machines that push also need the private **signing key** on them; and there is **no simple way to clean up an S3 cache based on access recency**.
- **Plain `ssh://` store**: pushing and pulling both require SSH access; pushers need the signing key and must be trusted by `nix-daemon`; single-machine, doesn't scale.
- **nix-serve / eris / harmonia**: these sign on the fly, but they serve from the local `/nix/store`, pushing still requires SSH as a trusted user, and they are single-machine and single-tenant. (harmonia serves your existing `/nix/store` over HTTP; it needs no separate storage but you must manage GC roots yourself to keep paths alive.)
- **Cachix**: excellent UX and central signing, and the direct inspiration for Attic's client UX — but it is a SaaS and cannot be freely self-hosted.

Attic's answer: a central server where **users push with a token** (no signing key on the client, no SSH, no S3 credentials on the client), signing is **managed centrally and done on the fly**, storage is deduplicated globally, and unused paths are **garbage-collected in an LRU manner** by recency of access. The FAQ is explicit that Attic **does not replace Cachix**: Cachix works at much larger scale and is a proven solution; Attic targets "personal or team use."

Relative to `nix copy --to s3://`, the key differences are: (a) clients authenticate with scoped JWTs rather than raw S3 keys; (b) the signing key stays on the server; (c) Attic adds a metadata database enabling multi-tenancy, per-cache config, and access-recency GC; and (d) Attic deduplicates via chunking, whereas a plain S3 cache stores each NAR whole.

### 2. Overall architecture
There are three binaries:
- **`atticd`** — the server. Runs the HTTP API server and/or the garbage collector, selected via `--mode`; in `monolithic` mode it runs everything, which is what the tutorial's one-command `atticd` startup does.
- **`attic`** — the client CLI. Works with multiple servers simultaneously (like git remotes): `attic login <name> <endpoint> <token>`, `attic cache create/configure/info`, `attic push`, `attic use`, `attic watch-store`. Cache references are `servername:cachename` or just `cachename` against the default server (set with `default-server` in `~/.config/attic/config.toml`).
- **`atticadm`** — server-side administration, principally `atticadm make-token` for minting JWTs. On NixOS the module installs a wrapper `atticd-atticadm` that runs as the `atticd` user with access to the server config/secret.

**Server components / modes:**
- `api-server`: stateless (auth is JWT-based, so no session state), therefore horizontally replicable behind a load balancer.
- `garbage-collector`: performs periodic GC on a configured interval; **cannot** be replicated (must be a singleton).

This split is the recommended production topology: multiple stateless API replicas + one GC worker.

**Database layer:** metadata (caches, store-path metadata, NAR records, chunk records, and the mapping tables that join them) is stored in a SQL database via SeaORM. **SQLite** (`sqlite:///…?mode=rwc`) is the zero-config default and is fine for single-node/personal use; **PostgreSQL** (`postgres://…`) is recommended for production and for running multiple API replicas. Migrations run automatically at startup (the tutorial shows "Running migrations… Migrating NARs to chunks… Migrating NAR schema…"), or can be run explicitly with `--mode db-migrations`.

**Storage backends:** configured under `[storage]` in `server.toml`:
- `type = "local"` with a `path` — stores chunk/NAR files on the local filesystem.
- `type = "s3"` with `bucket`, `region`, optional `endpoint` (for S3-compatibles such as Cloudflare R2, Backblaze B2, MinIO, or Garage), and `[storage.credentials]` (`access_key_id`, `secret_access_key`).

Zhaofeng's own instance ran on fly.io with a Neon PostgreSQL database and Cloudflare R2 object storage (R2 chosen for zero egress fees) — a good template for a serverless deployment.

### 3. Chunking and deduplication
This is Attic's signature feature. Deduplication is **global** (across all caches on the server) and operates at two granularities: whole **NAR files** and sub-file **chunks**.

**On upload:** when a NAR is uploaded, the server splits the **entire uncompressed NAR** into variable-size chunks using the **FastCDC** content-defined chunking algorithm (Xia et al., USENIX ATC 2016). FastCDC uses a rolling (gear) hash to choose cut-points based on content, so inserting or shifting bytes only changes the chunks around the edit rather than every chunk after it (it avoids the "boundary-shift problem" of fixed-size chunking). Identical chunks — identified by the hash of the **uncompressed** chunk — are stored only once in the backend. If an identical whole NAR already exists in the global NAR store, chunking is skipped entirely and the NAR is deduplicated directly.

**Compression pipeline:** the documented data flow is Chunk Stream → Chunk Hasher → Compressor → File Hasher → storage. Each chunk is hashed (uncompressed, for dedup), then compressed (zstd by default; xz/none also available), then the compressed file is hashed/sized and streamed to the backend. Deduplication keys on the uncompressed-chunk hash; the stored object is the compressed chunk.

**Chunking parameters** (`[chunking]` in `server.toml`):
- `nar-size-threshold` — minimum NAR size to trigger chunking. **`0` disables chunking entirely** for new NARs; **`1` chunks all NARs**.
- `min-size` — preferred minimum chunk size.
- `avg-size` — preferred average (target) chunk size; FastCDC normalizes the distribution toward this.
- `max-size` — preferred maximum chunk size.

The upstream `config-template.toml` defaults are `nar-size-threshold = 65536` (64 KiB), `min-size = 16384` (16 KiB), `avg-size = 65536` (64 KiB), `max-size = 262144` (256 KiB). (The docs' NixOS example page uses `avg-size = 131072`/128 KiB — the exact defaults have drifted between doc pages, so treat them as a starting point, not gospel.)

**Reassembly on download:** `atticd` reassembles the entire NAR from its constituent chunks by streaming them from the storage backend in order, decompressing, and concatenating, then serves the result. Because reassembly is server-side, the client sees an ordinary NAR.

**Why chunk the whole NAR instead of individual files?** Big NARs that benefit most from dedup (VSCode, Zoom, Electron apps) contain hundreds or thousands of tiny files; fetching thousands of individual objects to reconstruct a NAR would be impractical on object storage. Chunking the whole NAR lets you pick a larger average chunk size that lumps small files together and ignores file boundaries — the same approach `casync` takes. (This is a deliberate divergence from the Tvix store protocol, which chunks individual files.)

**Tradeoffs and limitations:**
- **CPU cost:** every upload is hashed, chunked, and compressed server-side; every download is reassembled server-side. This is CPU- and I/O-intensive.
- **Performance:** the default chunk sizes are small, producing many tiny objects, which can make throughput poor — especially on S3 backends. The Celler fork maintainer ("blitz") stated on May 12, 2026: "The suggested settings for chunking are way too small for good performance, especially with S3 as your backend, but likely also with local disks." Community reports describe upload/download throughput far below what the same hardware achieves over NFS/SMB/MinIO.
- **Changing parameters is costly:** the config warns that altering chunking values shifts the cut-points, so existing chunks can't be reused for new uploads and the **dedup ratio degrades for a while** after a change.
- **Fragility:** "When a chunk is deleted from the database, all dependent `.nar` will be deleted," and Attic "cannot automatically detect when a chunk is corrupt or missing." A missing/corrupt chunk therefore invalidates whole NARs, and repair tooling in `atticadm` was still described as future work.
- **Disabling chunking** (`nar-size-threshold = 0`) is a legitimate choice: you keep whole-NAR-level dedup and lose only sub-NAR dedup, trading storage efficiency for speed and robustness.

### 4. HTTP API and Nix binary-cache protocol compatibility
Attic exposes two distinct surfaces on the same server:

**(a) Standard Nix binary cache protocol**, served per-cache under `/{cache}/…`, so Nix can use it as an ordinary substituter:
- `GET /{cache}/nix-cache-info` → returns `StoreDir: /nix/store`, `WantMassQuery: 1`, `Priority: 41`.
- `GET /{cache}/{hash}.narinfo` → returns the signed narinfo, or `{"code":404,"error":"NoSuchObject",…}` if absent.
- `.nar` paths under the cache serve the NAR bytes.

The "Binary Cache Endpoint" is `http://host/<cache>` and the "API Endpoint" (for the client) is the server root `http://host/`. From the Nix daemon's perspective this is a normal HTTP binary cache: you add the endpoint to `substituters` and the cache's public key to `trusted-public-keys`, and Nix substitutes as usual (the user must be a `trusted-user` to pull into the real store).

For **downloads**, Zhaofeng described the design (in 2023) as not proxying data: the server returns a **307 redirect to a presigned S3 URL** so the client fetches bytes directly from object storage. Note this predates chunking; for chunked NARs, `atticd` must reassemble and stream server-side (as the FAQ states), so the redirect-to-presigned-URL path applies to the non-chunked/whole-object case. Uploads are always streamed through the server because compression and chunking are server-side.

**(b) Attic-specific client API**, under the versioned prefix **`/_api/v1/`**:
- **`/_api/v1/get-missing-paths`** — the **upload negotiation / dedup check**: the client sends the closure's paths and the server replies with which ones it lacks, so only missing paths are uploaded. This is what produces the `attic push` banner "(566 already cached, 2001 in upstream)."
- **`PUT /_api/v1/upload-path`** — the actual upload (handler `attic_server::api::v1::upload_path::upload_path` in `server/src/api/v1/upload_path.rs`). The NAR contents plus the store-path metadata (store path, references, deriver, NAR hash, NAR size, signature) are sent together; the server chunks, compresses, and stores, then records the local-cache→global-NAR mapping. A newer chunked variant handler (`upload_path_new_chunked`) also exists in the server source.
- **Cache management** (`create-cache`, `configure-cache`, `destroy-cache`) is likewise exercised over `/_api/v1/` by `attic cache create/configure/…`. (The exact route strings for these three weren't source-confirmable during research — see Caveats.)

**Proof-of-possession:** by default, even if a NAR already exists globally, the client must still fully upload it "to prove possession." Instead of writing to the backend, the server just runs the upload through a hash function and discards it, then creates the mapping. Setting **`require-proof-of-possession = false`** in the config lets such uploads short-circuit to immediate success (faster, but a client could then claim access to a NAR it doesn't actually have). Malicious/incorrect metadata only pollutes the uploader's own cache, because path metadata is per-cache while the global store holds only "context-free," content-addressed NARs and chunks.

The client does the heavy lifting via an async Rust binding to the C++ `libnixstore`, which lets it compute closures, look up path metadata, and stream NARs. (These FFI bindings are exactly what the Celler fork found fragile and replaced with the native-Rust `nix-daemon` crate.)

### 5. Authentication and access control
Auth is **stateless**: a signed JWT carries all permissions, so any `api-server` replica can verify a request without shared session state. The token's custom claim namespace is `https://jwt.attic.rs/v1`, under which a `caches` object maps cache-name patterns to permission bits (e.g. `r`=pull, `w`=push, `cc`=create-cache).

**Signing algorithms / secrets:** the signing secret is provided base64-encoded via environment variables:
- `ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64` — an HS256 (HMAC-SHA256) symmetric secret; generate with `openssl rand 64 | base64 -w0`.
- `ATTIC_SERVER_TOKEN_RS256_SECRET_BASE64` — an RS256 (RSA) private key; generate with `openssl genrsa -traditional 4096 | base64 -w0`. HS256/RS256 support was added in November 2023 (commit by Cole Helbling, "server: support HS256, RS256 JWT secrets"); the NixOS deployment docs now default to RS256.

**Permissions** (set by `atticadm make-token` flags, each accepting a cache-name pattern that may contain wildcards):
- `--pull` (r), `--push` (w), `--delete`, `--create-cache` (cc), `--configure-cache`, `--configure-cache-retention`, `--destroy-cache`.
- Plus `--sub` (subject/identity), `--validity` (e.g. `"3 months"`, `"2y"`), and `--dump-claims` to preview claims without encoding.

**Wildcards / namespacing:** a token scoped to `alice-*` for pull/push/create-cache lets Alice create and use any cache whose name starts with `alice-` — self-service tenancy without an admin creating each cache. The root token printed on first run is all-powerful and must not be shared.

**Public vs private:** `attic cache configure <cache> --public` grants unauthenticated pull access (so `curl …/nix-cache-info` works with no token); caches are private by default. Tenants are mutually untrusting and cannot see or pollute each other's views.

**Known weaknesses:** the community (and the Celler maintainer) flag long-lived JWTs as a footgun — there's no revocation short of rotating the signing key, and passing tokens on the command line leaves them in shell history. OIDC + short-lived tokens is the widely-requested fix, planned in Celler but not present in upstream Attic.

### 6. Signing and client trust
Each cache has its **own Nix signing keypair**, generated and held server-side. When a client fetches a path, `atticd` signs the narinfo **on the fly** — the crucial property is that **users who push never possess the signing key**, so a compromised pusher can't forge signatures for arbitrary content. `attic cache info` shows the cache's public key (e.g. `hello:vlsd7ZHIXNnKXEQShVnd7erE8zcuSKrBWRpV6zTibnA=`).

**Client trust:** `attic use <cache>` writes `~/.config/nix/nix.conf`, adding the cache to `substituters`, the cache's public key to `trusted-public-keys`, and the access token to the netrc so private caches can be pulled. You can also configure Nix manually from `attic cache info` output. In CI, the common pattern is `attic login` + writing `/etc/nix/netrc` with the token + adding the substituter and public key to Nix settings.

### 7. Garbage collection and retention
Attic supports **LRU-style, access-recency GC** — the thing plain S3/SSH caches lack. Retention is configured per cache: `attic cache configure <cache> --retention-period '90d'` deletes objects not accessed within the window; `--reset-retention-period` reverts to the global default (which by default is "do not GC"). A global default and the GC `interval` (e.g. `"12 hours"`) live under `[garbage-collection]` in `server.toml`.

Because of global dedup, GC cascades through **three levels**:
1. **Local cache:** only the mapping between the local cache's metadata and the global NAR is deleted. The cache loses access, but **no storage is freed**.
2. **Global NAR store:** NARs no longer referenced by any local cache become eligible for deletion.
3. **Global chunk store:** chunks no longer referenced by any NAR become eligible for deletion — **this** is where bytes are actually freed, and a subsequent upload of the same chunk will re-upload it.

GC runs periodically in the `garbage-collector` mode (a non-replicable singleton) or can be triggered once with `atticd --mode garbage-collector-once`. Access tracking (last-accessed timestamps) drives the LRU behavior. A notable missing feature is **GC pinning / GC roots** — there's no way to mark a released artifact as "never collect," a frequently requested feature tracked in the Celler fork.

### 8. Deployment in practice
**`server.toml` key sections:**
- `listen` — socket address (e.g. `[::]:8080`); `allowed-hosts`; optional `api-endpoint`.
- `token-hs256-secret-base64` (or provide via env var), plus the `[jwt]` section.
- `[database] url` — `sqlite:///…?mode=rwc` or `postgres://…`.
- `[storage]` — `type = "local"` + `path`, or `type = "s3"` + `bucket`/`region`/`endpoint` + `[storage.credentials]`.
- `[chunking]` — the four parameters above.
- `[compression] type` — `zstd` (default), `xz`, `none`, with optional `level`.
- `[garbage-collection] interval` — GC cadence; plus default retention.

**NixOS module:** import `attic.nixosModules.atticd` (flakes) or `nixos/atticd.nix`, then set `services.atticd.enable`, `environmentFile` (holding the secret env var, root-readable only), and `settings` (mirroring `server.toml`). The module auto-detects a local PostgreSQL and installs the `atticd-atticadm` wrapper. Put it behind an NGINX (or similar) reverse proxy for HTTPS; there's a known long-request/timeout consideration for large uploads (e.g. Traefik's `readTimeout=0s`).

**Docker/Docker Compose:** commonly run as a container (there was a `ghcr.io/zhaofengli/attic` image) with a Postgres sidecar, an env file carrying `ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64` and `DATABASE_URL`, and a mounted `server.toml`; then `atticadm make-token … -f ./server.toml` to mint the first token.

**Migration considerations:** migrations run automatically at startup (or via `--mode db-migrations`). The project has historically warned that "you might even be required to reset the entire database," so back up before upgrading. Moving from an older pre-chunking version requires adding the `[chunking]` section. There is no `atticd`/`attic-server` package in nixpkgs (only the `attic`/`attic-client`); building the server can be memory-hungry, which bites low-RAM ARM machines.

**Operational gotchas / status warnings:**
- Persistent **"early prototype … APIs may be changed without backward-compatibility"** warnings in official docs.
- Small default chunk sizes → poor throughput; tune or disable chunking.
- **Large NARs can OOM the client** — greg-hellings reported (NixOS Discourse, May 3, 2026) that a ~120 GB single-file NAR (an English Wikipedia ZIM file) crashed the client "because it was reading the entire file into memory at 2 or 3 separate points during the upload, and my build machines do NOT have 360GB of RAM." He maintains a downstream client patch.
- No built-in metrics/observability, which the Celler fork calls out as a top pain point: "there is no good way to troubleshoot an Attic instance beyond instrumenting the code" (blitz, April 27, 2026).
- I found **no published CVE** specific to Attic; the security concerns discussed are design-level (long-lived JWTs, the proof-of-possession trade-off) rather than assigned vulnerabilities.

### 9. Project status in 2026 and alternatives
The upstream **`zhaofengli/attic`** repo (roughly 1.7k–2k stars, ~473 commits, Apache-2.0) is **still functional and widely used but effectively stalled**: it has no tagged releases, is still self-described as an early prototype, and by early 2026 the community described "7 months of no activity." Zhaofeng Li also co-created Determinate Systems' Magic Nix Cache, which built on his Attic work.

The most direct response is **Celler**, a fork announced **April 27, 2026** by "blitz" at Cyberus Technology, explicitly created because "there is an Attic binary cache fork that aims to continue the stalled Attic development (7 months of no activity)." The motivation was concrete: "while Attic roughly saves us 60% of storage costs, it also massively increases our maintenance costs. Not a win!" Celler keeps the **same database schema** (users report migrating with no data changes by just pointing Celler at the existing Attic DB), and its roadmap is production-hardening: it **replaced the fragile C++ `libnixstore` FFI with the native-Rust `nix-daemon` crate** ("we now use native Rust code to talk to the Nix daemon (via gorgon's nix-daemon crate) where possible and shell out to Nix CLI where necessary"), and prioritizes logging/metrics (OpenTelemetry/Prometheus requested), then OIDC auth, GC pinning, cache listing, and secret-loading via systemd credentials.

Other tools people are moving to or evaluating:
- **ncps** (kalbasit) — a Nix cache **proxy** with local caching and signing; pulls from upstreams like cache.nixos.org and caches locally; supports SQLite/PostgreSQL/MySQL/Redis for HA. Different niche (transparent pull-through proxy) but overlaps with Attic for many users; still self-described as early-stage with data-loss warnings.
- **niks3** (Mic92) — an S3-backed cache with GC and OIDC that deliberately **does not deduplicate**. Per zimbatm (May 1, 2026): "We found that uploads are much faster than anything else out there. It's still leaning on S3 for the read path, so niks3 doesn't become a single point of contention for the read path. It also doesn't try to do de-duplication."
- **harmonia** (nix-community) — Rust server that exposes your existing `/nix/store` over HTTP; simplest if you don't need multi-tenancy/dedup and can manage GC roots.
- Plain **`nix copy --to s3://`** with lifecycle rules — simplest of all if you don't need tenancy, tokens, or dedup.

## Recommendations
1. **For a personal or small-team private cache, Attic is still a strong choice today.** Start in `monolithic` mode with SQLite + local storage to learn it (the 15-minute tutorial), then graduate to PostgreSQL + S3-compatible storage (R2/B2/MinIO/Garage) behind NGINX for HTTPS, splitting `api-server` and `garbage-collector` modes.
2. **Tune chunking before you care about performance.** The stock chunk sizes are too small for S3 and hurt throughput. Either raise `avg-size`/`max-size` substantially or set `nar-size-threshold = 0` to disable sub-NAR chunking (you keep whole-NAR dedup). Decide this *before* uploading a lot, because changing parameters later tanks the dedup ratio on new uploads.
3. **Treat tokens as long-lived secrets.** Mint narrowly-scoped tokens (`--pull`/`--push` on specific cache-name patterns, realistic `--validity`), keep the root token offline, store the signing secret root-only (systemd `EnvironmentFile`, agenix/sops), and remember that revocation means rotating the signing key. Avoid passing tokens on the CLI in shared shells.
4. **Back up the database and set retention early.** Enable `[garbage-collection]` with a sane `interval` and per-cache `--retention-period`, and snapshot the DB before every upgrade (historically breaking).
5. **For new *production* deployments in 2026, seriously evaluate Celler** as a drop-in (same schema, active maintenance, native Nix daemon comms, metrics coming), and consider **niks3** if you value upload speed and OIDC over storage-saving dedup, or **ncps** if what you actually need is a caching pull-through proxy. Migrating from Attic to Celler currently requires only pointing the new server at the existing DB — but test on a copy first.

**Thresholds that should change the decision:**
- If you push **very large single-file NARs** (tens of GB), test client memory use first or patch the client — upstream Attic can OOM (the 120 GB case above).
- If you need **observability/metrics, OIDC, GC pinning, or cache listing**, upstream Attic won't provide them — move to Celler or another tool.
- If your dedup savings are small (measure it — Cyberus saw ~60% storage savings, which justified the complexity for them), the operational cost of chunking may not be worth it; a plain S3 cache or niks3 may be simpler.

## Caveats
- **Prototype status / API instability:** Attic's own docs still warn that APIs may change without backward-compatibility and that a full database reset could be required; there are no tagged releases.
- **Source-verification gaps:** the exact route strings for `create-cache`/`configure-cache`/`destroy-cache` under `/_api/v1/`, the precise upload-path metadata preamble/headers, and whether any `X-Attic-*` headers are used could not be confirmed from source (GitHub raw files were bot-blocked during research). The `get-missing-paths` and `PUT /_api/v1/upload-path` routes and the `upload_path` handler are confirmed from server logs quoted in issues. The "307 redirect to presigned S3 URL" for downloads comes from the author's 2023 Discourse comments and predates chunking, so it applies to the non-chunked path; chunked NARs are reassembled and streamed server-side.
- **Shifting defaults:** documented default chunk sizes differ between the docs pages and the config template (e.g. `avg-size` of 64 KiB vs 128 KiB); verify against your version's `config-template.toml`.
- **Maintenance is a moving target:** the stalled/forked situation is as of mid-2026 and could change (upstream could revive, or Celler could become the de-facto successor). Re-check current repo activity before committing.
- **Benchmarks are anecdotal:** the throughput complaints and the ~60% storage-savings figure come from individual community reports, not controlled benchmarks; your mileage will vary with backend, network, and chunking configuration.
