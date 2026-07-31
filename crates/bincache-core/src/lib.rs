//! Domain vocabulary and pure computation for the Nix binary cache protocol.
//!
//! No I/O, no async, no sibling crate dependencies. Everything here is a total or fallible
//! function over values, which is what makes it the property-testable functional core
//! described in `research/DESIGN_V2.md` under "Functional core, imperative shell".

pub mod base32;
pub mod cacheinfo;
pub mod compression;
pub mod hash;
pub mod narinfo;
pub mod sign;
pub mod storepath;
