//! The write path: authenticated, latency-tolerant, and the only writer to the index.
//!
//! Every expensive computation in the system happens here, exactly once per path:
//! verification, compression, rendering, and signing. See `research/DESIGN_V2.md`,
//! "The ingest plane".

pub mod auth;
pub mod ingest;
pub mod upload;
