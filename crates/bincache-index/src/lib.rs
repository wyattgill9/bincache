//! The metadata plane's ground truth: an embedded key-value store holding narinfo records.
//!
//! Records are stored as fields, `rkyv`-encoded, not as rendered bytes. That is what lets
//! the render format change and the signing key rotate without a data migration, and it is
//! why `bincache-serve` owns HTTP framing entirely. See `research/DESIGN_V2.md`, "The
//! metadata plane".
//!
//! The RAM projection the design describes is deliberately absent in v1: it would be a
//! derived, non-authoritative cache, and there is nothing profiled yet to justify it.

pub mod index;
pub mod nar;
