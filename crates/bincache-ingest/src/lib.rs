//! The write path: authenticated, latency-tolerant, and the only writer to the data
//! directory.
//!
//! Every expensive computation in the system happens here, exactly once per path:
//! verification, compression, rendering, and signing. What the read path serves is what
//! this produced.

pub mod auth;
pub mod fault;
pub mod ingest;
pub mod maintain;
pub mod upload;
