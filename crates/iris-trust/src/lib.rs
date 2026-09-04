//! Decoder identity, content hashes and substitution policy.
//!
//! A decoder is named by a URI and pinned by a BLAKE3 digest. A host that recognises the digest may run its own native implementation instead, and a host that does not may fetch and verify.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
