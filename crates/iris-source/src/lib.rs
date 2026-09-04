//! Range oriented data sources for iris.
//!
//! A decoder declares the byte ranges it needs and the host serves them. That inversion is what lets the same decoder run against a local file, a page cache, and an object store.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
