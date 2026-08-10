//! The data directory: artifacts, published narinfo bodies, and NAR receipts.
//!
//! Everything durable lives here and nothing else owns any of it. A publish is a rename, so
//! there is no snapshot to validate at boot, no log to replay, and no second commit to keep
//! in step with the first.

pub mod atomic;
pub mod nar;
pub mod narinfo;
pub mod receipt;
