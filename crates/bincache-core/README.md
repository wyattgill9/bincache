# bincache-core

Domain vocabulary and pure computation. No I/O, no async, no sockets, no filesystem, no
sibling crate dependencies.

## Owns

- **Canonical types.** Every value the other crates speak in: `StorePathHash([u8; 20])`
  decoded from the 32-character base32 request key, `NarHash`, `FileHash`, `Signature`,
  `Compression`, `NarInfo`, push-credential tokens. Parsed into newtypes at construction,
  so no downstream crate can hold an unvalidated one.
- **Pure functions.** Narinfo render, the full pre-rendered HTTP response blob (status
  line, headers, signed body) that the index stores and the serving path writes verbatim,
  ed25519 sign and verify, base32 decode.

## Does not own

Reading or writing bytes anywhere. The no-I/O rule is load-bearing: this crate is the
functional core that deterministic simulation testing exercises without a mock driver, and
the property-test surface (render then parse is identity, sign then verify is ok, base32
round-trip).

## Depends on

Nothing in this workspace.

## Design references

DESIGN.md: "Parse, don't validate — at the socket", "Pre-rendered responses in a huge-page
arena", "Functional core, imperative shell".
