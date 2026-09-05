//! The guest and host ABI for iris self-decoding datasets.
//!
//! The ABI is the only surface in iris that can ossify, so it is shaped like a wire protocol: length prefixed records, negotiated capabilities, and a defined way for either side to refuse politely.
//!
//! This crate is `no_std` and has no dependencies at all, and both of those are checked by CI. It
//! ends up inside every decoder anyone writes, so anything it pulls in, everyone pays for.
//!
//! # How a conversation goes
//!
//! The host sends a [`Hello`] saying which ABI version it speaks and what it can do. The decoder
//! answers with a [`HelloAck`] saying what it needs. [`negotiate`] compares the two and either
//! produces an [`Agreement`] or a [`Refusal`] that names the capability that was missing. After
//! that the host sends [`ScanRequest`] records and the rows come back as [`Batch`] records. When the
//! decoder needs bytes it has not been given it asks for them, as a [`RangeRequest`] record where
//! there is a channel to put one on and as a call answered by a [`RangeStatus`] where the decoder is
//! stopped inside the request waiting for it.
//!
//! ```
//! use iris_abi::{Capability, CapabilitySet, Hello, HelloAck, negotiate};
//!
//! let host = Hello {
//!     abi_major: iris_abi::ABI_MAJOR,
//!     abi_minor: iris_abi::ABI_MINOR,
//!     window_bytes: 64 << 20,
//!     max_batch_rows: 8192,
//!     offered: CapabilitySet::new()
//!         .with(Capability::REQUIRE_RANGE)
//!         .with(Capability::SLIDING_WINDOW),
//!     source_bytes: 4 << 30,
//! };
//! let decoder = HelloAck {
//!     abi_major: iris_abi::ABI_MAJOR,
//!     abi_minor: iris_abi::ABI_MINOR,
//!     required: CapabilitySet::new().with(Capability::REQUIRE_RANGE),
//!     optional: CapabilitySet::new().with(Capability::PROJECTION),
//!     decoder_id: "example",
//! };
//!
//! let agreed = negotiate(&host, &decoder).expect("the host offers what the decoder needs");
//! assert!(agreed.has(Capability::REQUIRE_RANGE));
//! // The host offers sliding windows but this decoder never asked for them, so it is not on.
//! assert!(!agreed.has(Capability::SLIDING_WINDOW));
//! // The decoder would like projection pushdown but this host does not do it, and that is fine
//! // because it was optional.
//! assert!(!agreed.has(Capability::PROJECTION));
//! ```
//!
//! # What is allowed to change
//!
//! Adding a field to the end of a record is allowed and does not bump that record's version, because
//! a reader that does not know about the field steps over it. Adding a record is allowed, because a
//! reader that does not know a tag steps over the whole record. Adding a capability is allowed,
//! because a side that does not offer it says so and the other side decides what to do.
//!
//! Removing a field, reordering fields, changing what a field means, or changing what a capability
//! bit means are all breaking, and all of them are supposed to be loud rather than silent. The
//! tests in `tests/forward_compat.rs` hold the compatible half of that line.

#![no_std]
// Nothing in here parses untrusted bytes with a pointer. If a future change needs to, it needs a
// conversation first, because this crate is the one piece of iris that runs inside every decoder
// anybody writes.
#![forbid(unsafe_code)]

pub mod caps;
pub mod error;
pub mod handshake;
pub mod message;
pub mod range;
pub mod record;
pub mod wire;

pub use caps::{Capability, CapabilitySet};
pub use error::{Error, Result};
pub use handshake::{Agreement, negotiate};
pub use message::{
    Batch, BufferRef, Buffers, Hello, HelloAck, Message, Node, Nodes, Projection, RangeRequest,
    Refusal, RefusalReason, ScanRequest,
};
pub use range::RangeStatus;
pub use record::{Header, Tag};
pub use wire::{Reader, Writer};

/// The major ABI version this build speaks.
///
/// Zero means the record layouts are still allowed to move. When this goes to one it means the
/// layouts in [`message`] are frozen, and freezing them is a milestone with a written compatibility
/// note behind it rather than something that happens because a refactor felt finished.
pub const ABI_MAJOR: u16 = 0;

/// The minor ABI version this build speaks.
///
/// This goes up when a field or a record or a capability is added. Two sides at different minor
/// versions can always talk to each other, and they settle on the lower of the two.
pub const ABI_MINOR: u16 = 3;
