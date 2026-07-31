# bincache-core

Domain vocabulary and pure computation. No I/O, no async, no sockets, no filesystem, no
sibling crate dependencies.

## Owns

- **Canonical types.** `storepath::{Hash, Path, Name, Dir}`, `hash::Sha256`,
  `compression::Compression`, `sign::{SecretKey, PublicKey, Signature}`, and the
  `narinfo::NarInfo` record. Parsed into newtypes at construction, so no downstream crate
  can hold an unvalidated one.
- **Pure functions.** Nix base32 in both directions, narinfo render and parse, the
  canonical fingerprint, ed25519 sign and verify, and the `nix-cache-info` body.
- **The NAR URL convention.** `narurl` is the single owner of `nar/<file hash>.nar<ext>`,
  both directions. Four places have to agree on it: the `URL:` field, the filename on disk,
  the route, and the `PUT` target that states what its body must hash to.

## Does not own

Reading or writing bytes anywhere. The no-I/O rule is load-bearing: this crate is the
functional core, so its behaviour is testable without a driver, a socket, or a temp
directory.

## Depends on

Nothing in this workspace.

## Notes

The record stores fields, not rendered bytes. That is what lets the render format change
and the signing key rotate without a data migration, and it is why HTTP framing lives
entirely in `bincache-serve`.

## Design references

DESIGN_V2.md: "Protocol surface", "Parse, don't validate, at the socket", "What gets
prerendered", "Managed signing", "Functional core, imperative shell".
