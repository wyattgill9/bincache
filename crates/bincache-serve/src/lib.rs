//! The read path: an axum router over an immutable index.
//!
//! HTTP framing lives entirely in hyper. Nothing durable encodes it, which is what keeps
//! HTTP/2 a configuration change rather than a data migration, and what keeps the framing
//! ambiguities that cause request smuggling out of this crate's fault domain.
//!
//! There is no auth on the read path at all, so a push credential structurally cannot
//! become a read-path dependency.

pub mod handler;
pub mod range;
pub mod route;
pub mod serve;
pub mod stats;
