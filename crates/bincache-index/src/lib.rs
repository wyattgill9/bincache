//! The metadata plane's ground truth: an embedded key-value store holding narinfo records.
//!
//! Records are stored as fields, `rkyv`-encoded, and the body a `GET` answers with is
//! projected beside each one at publish. The record stays the source of truth, so the
//! signing key still rotates by rewriting records, and `bincache-serve` still owns HTTP
//! framing entirely. See `research/DESIGN_V2.md`, "The metadata plane".
//!
//! Rendering per request was measured at 1.1 us against a 0.8 us lookup, which is what
//! moved it to publish time. The RAM projection the design describes would remove the
//! remaining lookup and is still absent: it costs about 1.2 KB per path and needs
//! invalidation across shards, neither of which the measured gap yet justifies.

pub mod index;
pub mod nar;
