# The Nix binary cache protocol, from the bottom

What NAR, narinfo, and the store path hash actually are, and which part of bincache owns
each one. Read this before `DESIGN.md` if the vocabulary is not yet automatic.

The protocol is small. Three HTTP routes, one archive format, one text manifest, one
signature scheme. Everything in `ARCHITECTURE.md` is an engineering response to the shape
described here.

## The store path

```
/nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1
           |<-------- 32 characters ------>| |<- name ->|
```

The 32-character prefix is **20 bytes** in Nix's own base32 alphabet,
`0123456789abcdfghijklmnpqrsvwxyz`. It drops `e`, `o`, `u`, and `t` so that generated
paths do not contain accidental words. 160 bits at 5 bits per character is exactly 32
characters, so there is no padding and no length ambiguity.

Those 20 bytes are a full SHA-256 folded in half by XOR (`compressHash` in the Nix
sources). The input to that SHA-256 is a description of how the path was *produced*: the
derivation, its inputs, and the output name. Nothing about the built content enters it.

That last point is the reason binary caches work at all. Nix computes the store path it
wants before any build runs, then asks a server whether that path already exists. If yes,
download. If no, build locally.

The 20-byte value is the primary key of the whole protocol. It is why `core` owns
`StorePathHash([u8; 20])` and why `serve` decodes 32 characters into one at step 2 of the
hot path, rejecting malformed input with a 400 before touching any data structure.

## NAR, the Nix ARchive

A serialization of a filesystem tree. Comparable to tar, reduced until the output is a
pure function of the content.

A NAR records three node types:

- **directory**, with entries sorted by name
- **regular file**, with its contents and exactly one permission bit, executable or not
- **symlink**, with its target string

A NAR deliberately discards mtimes, uid and gid, every mode bit except the executable one,
xattrs, and hardlink identity. Each of those varies between machines that built the same
thing, so keeping them would break the property the format exists to provide: the same
tree serializes to the same bytes anywhere. `sha256(nar_bytes)` is therefore a stable
content identity, and that number is the **NarHash**.

The encoding is length-prefixed strings (`u64` little-endian, zero-padded to an 8-byte
boundary) in a parenthesized structure, opening with the magic string `nix-archive-1`. It
is small enough to implement completely from the spec.

Two hashes are involved per path and confusing them is the standard mistake:

| Name | Computed over | Names the file on disk |
|---|---|---|
| `NarHash` | the uncompressed NAR bytes | no |
| `FileHash` | the compressed artifact actually downloaded | yes |

A client downloads a file, checks `FileHash`, decompresses, checks `NarHash`, then
unpacks. `FileHash` naming the artifact is why the payload route is
`/nar/<filehash>.nar.zst`.

## narinfo

A plain-text manifest describing one store path, one `Key: Value` per line. This is the
object the entire bincache read path exists to return:

```
StorePath: /nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1
URL: nar/1w1fff338fvdw53sqgamddn1b2xgds473vpqlgh8blp3qgcasjd.nar.zst
Compression: zstd
FileHash: sha256:1w1fff338fvdw53sqgamddn1b2xgds473vpqlgh8blp3qgcasjd
FileSize: 50088
NarHash: sha256:1impfw8zdgisxkghq9a3q7cn7jb9zyzgxbcqz71gcvw62lyefwsq
NarSize: 226504
References: 5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1 kzp5qfy8...-glibc-2.37
Deriver: 9fs4vq4gdsb8r9ywawq5c9dfvj7lp5g8-hello-2.12.1.drv
Sig: cache.nixos.org-1:PBmH1n0X/8j2Z...==
```

`References` carries the store paths this path depends on at runtime, and it drives client
behaviour more than any other field (see "Closure resolution" below). A path that
references itself is normal: it means the path's own hash string appears somewhere inside
its own files.

Every field is determined the moment an upload finishes. None of it varies per request.
That is the concrete justification for the pre-rendered response blob in
`ARCHITECTURE.md`, and it holds.

## The signature

`Sig` is ed25519 over a canonical string called the **fingerprint**:

```
1;<store path>;sha256:<narhash in base32>;<narsize>;<comma-joined reference paths>
```

The client verifies it against the public keys in its `trusted-public-keys` setting. The
fingerprint is built only from immutable narinfo fields, so the signature is computable
once at ingest alongside the rest of the blob. That is why the typestate chain puts
signing in the `Upload<Stored>` to `Published` transition, and why `publish()` exists only
on a state whose verify step has already run.

The corollary that arrives later: rotating the signing key invalidates every stored
signature, so `store` must persist enough metadata to regenerate and re-sign every blob
without re-reading the NARs.

## What a binary cache is, as a server

Three routes. No auth, no negotiation, no session state. The shape is static-file-like,
which is why an S3 bucket can serve as one.

| Route | Returns |
|---|---|
| `GET /nix-cache-info` | three lines: `StoreDir`, `WantMassQuery`, `Priority` |
| `GET /<32-char-hash>.narinfo` | the manifest above, or 404 |
| `GET /nar/<filehash>.nar.zst` | the compressed NAR |

`Priority` breaks ties when several configured caches hold the same path, lower winning.
`cache.nixos.org` sits at 40. `WantMassQuery` tells the client whether bulk narinfo
queries against this cache are welcome.

Optional surface exists beyond the three: `/<hash>.ls` JSON file listings, and `HEAD` on
the narinfo route, which `nix copy` uses as an existence check before deciding what to
upload. `HEAD` reuses the header segment of the same pre-rendered blob.

## Closure resolution, and why misses dominate

A client that wants to realise something:

1. Evaluates, and computes the store paths of the closure it needs. Several hundred is
   ordinary.
2. Issues `GET /<hash>.narinfo` for all of them, in parallel.
3. For each 200, reads `References`, adds any path it has not seen, and repeats from 2.
4. For each path that resolved, fetches the NAR at `URL` and unpacks it.
5. For each 404, builds locally.

So one client action produces hundreds of metadata queries, and for any cache holding a
slice of nixpkgs rather than all of it, most of those return 404. Meanwhile the payload
plane only ever receives requests for paths whose narinfo already resolved, so it sees
hits and nothing else.

Those two opposite profiles are the organizing fact behind the plane split in
`ARCHITECTURE.md`. They are also what makes a binary fuse filter in front of the index the
right structure: it makes "no, not here" cost close to nothing, and that answer is the
common one.

## Ownership map

| Protocol concept | bincache owner |
|---|---|
| base32 decode, `StorePathHash`, `NarHash` | `core` |
| narinfo rendering, fingerprint, ed25519 sign | `core` |
| 20-byte key to finished blob bytes | `index` |
| `.nar.zst` files, named by `FileHash` | `store` |
| receive, verify `NarHash`, compress, sign, publish | `ingest` |
| the three routes | `serve` |

## One correction to ARCHITECTURE.md

"A narinfo response is fully built at upload time" is correct about the **body**. It is
not correct about the **headers**. `Content-Length` is fixed per path, but Range requests,
conditional `304` responses, and HTTP/2 header compression each mean the response frame
cannot be written byte-identically for every request. Bake the body, generate the header.
That is the narrow version of the pre-rendering row in the unresolved fork table, and it
is cheap to settle now and expensive to settle after `serve` exists.

## Sources

- `nix/src/libutil/hash.cc`, `compressHash` and the base32 alphabet
- `nix/src/libutil/archive.cc`, the NAR format
- `nix/src/libstore/path-info.cc`, `ValidPathInfo::fingerprint`
- `nix/src/libstore/binary-cache-store.cc`, the route set
