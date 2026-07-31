//! The read path: thread-per-core serving shards over an immutable index.
//!
//! No mutable shared state on the serving path, no auth, and no allocation per metadata
//! request beyond the rendered body itself. See `research/DESIGN_V2.md`, "The payload
//! plane" and "Execution model: thread-per-core on Compio".
//!
//! HTTP framing lives entirely here. Nothing durable encodes it, which is what keeps
//! HTTP/2 a change to this crate rather than a data migration.

pub mod connection;
pub mod handler;
pub mod http;
pub mod range;
pub mod route;
pub mod shard;
pub mod stats;
