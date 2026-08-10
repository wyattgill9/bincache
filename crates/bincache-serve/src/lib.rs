//! The read path: an axum router over a directory of immutable files.
//!
//! Answering `GET /<hash>.narinfo` is an `open` and a `write`. The body was rendered and
//! signed once, at publish, from fields ingest verified itself, and the bytes on disk are
//! the bytes on the wire: nothing is decoded, re-rendered, or re-signed per request.
//!
//! HTTP framing lives entirely in hyper. Nothing durable encodes it, which keeps HTTP/2 a
//! configuration change rather than a data migration, and keeps the framing ambiguities
//! that cause request smuggling out of this crate's fault domain.
//!
//! There is no auth on the read path at all, so a push credential structurally cannot
//! become a read-path dependency.

pub mod handler;
pub mod range;
pub mod route;
pub mod serve;
pub mod stats;
