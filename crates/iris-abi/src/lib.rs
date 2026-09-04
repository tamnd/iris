//! The guest and host ABI for iris self-decoding datasets.
//!
//! The ABI is the only surface in iris that can ossify, so it is shaped like a wire protocol: length prefixed records, negotiated capabilities, and a defined way for either side to refuse politely.
//!
//! Nothing is implemented yet. See the milestone that owns this crate in
//! `docs/ROADMAP.md`.

/// The ABI revision this build of iris speaks.
///
/// Bumping this is a deliberate act with a written compatibility note, not a
/// side effect of a refactor.
pub const ABI_VERSION: u32 = 2;
