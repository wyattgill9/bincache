//! Configuration, boot, and wiring. The only crate that knows the other five exist
//! together.
//!
//! No behaviour lives here beyond assembling it: every decision this crate makes is which
//! handle to hand to which sibling.

pub mod args;
pub mod boot;
pub mod watchdog;
