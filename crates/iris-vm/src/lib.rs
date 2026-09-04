//! WebAssembly execution layer for iris decoders.
//!
//! Wraps Wasmtime. Owns instantiation, execution metering, the state page, and the sliding window that lets a wasm32 guest address a dataset larger than four gibibytes.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
