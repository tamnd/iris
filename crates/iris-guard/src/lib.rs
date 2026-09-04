//! Structural validation of Arrow arrays crossing the sandbox boundary.
//!
//! A sandboxed decoder returns offsets, lengths and validity bitmaps that the host is about to trust. This crate checks them first, and the cost of checking them is a published number.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
