//! Bundle and metadata format for iris self-decoding datasets.
//!
//! Parses and writes the container that carries a dataset, the decoder reference, and the content digest that lets a host substitute a native decoder it already trusts.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
