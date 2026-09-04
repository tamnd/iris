//! Guest side SDK for writing iris decoders.
//!
//! A decoder is a `wasm32-unknown-unknown` module that speaks the ABI in
//! [`iris_abi`]. This crate hides the record encoding and the host imports
//! behind a macro so that writing a decoder is mostly writing the decode loop.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
