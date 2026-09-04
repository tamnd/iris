//! Native fast path implementations for known decoders.
//!
//! The WebAssembly vector width is capped at 128 bits and will be for the foreseeable future, so a host that recognises a decoder should be able to skip the sandbox. This is that table.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
