//! The filesystem as the payload database.
//!
//! NAR artifacts are immutable files at content-addressed names, so the page cache is the
//! payload cache and recovery only ever has to answer "present or absent", never "which
//! version". See `research/DESIGN_V2.md`, "The payload plane".

pub mod nar;
