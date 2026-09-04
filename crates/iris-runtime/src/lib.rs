//! The iris runtime an engine embeds.
//!
//! Ties the format, the virtual machine, the source, the guard and the native table into a scan an engine can pull batches from.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
