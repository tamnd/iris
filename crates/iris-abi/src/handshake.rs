//! Working out whether the host and the decoder can work together, and saying so plainly when they
//! cannot.
//!
//! The thing this is trying to avoid is a decoder that runs and produces wrong answers because the
//! host quietly ignored something it did not understand. Every path through here either ends in an
//! agreement that both sides can name, or in a refusal that says which bit was the problem.

use crate::caps::{Capability, CapabilitySet};
use crate::message::{Hello, HelloAck, Refusal, RefusalReason};

/// What the two sides agreed on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Agreement {
    /// The major version both sides speak.
    pub abi_major: u16,
    /// The minor version both sides speak, which is the lower of the two.
    pub abi_minor: u16,
    /// The capabilities the host offers and the decoder asked for, required or optional.
    ///
    /// A capability the host offers and the decoder never mentioned is not in here, so the host can
    /// use this to decide what it actually has to set up.
    pub agreed: CapabilitySet,
    /// Carried over from the host's [`Hello`] so the caller has one thing to hold on to.
    pub window_bytes: u64,
    /// Carried over from the host's [`Hello`].
    pub max_batch_rows: u64,
}

/// Decides whether a host and a decoder can work together.
///
/// # Errors
///
/// Returns the [`Refusal`] that should be sent to the other side. The detail strings are fixed
/// rather than formatted, because this crate does not allocate, and the machine-readable part of a
/// refusal is the reason code and the capability anyway.
pub fn negotiate(hello: &Hello, ack: &HelloAck<'_>) -> Result<Agreement, Refusal<'static>> {
    if ack.abi_major > hello.abi_major {
        return Err(Refusal::new(
            RefusalReason::ABI_TOO_NEW,
            "the decoder was built against a later major version of the iris ABI than this host speaks",
        ));
    }
    if ack.abi_major < hello.abi_major {
        return Err(Refusal::new(
            RefusalReason::ABI_TOO_OLD,
            "the decoder was built against an earlier major version of the iris ABI than this host speaks",
        ));
    }

    // A decoder built against a later minor version can require a capability that did not have a
    // name when this host was compiled. Truncating the bitset would turn that into "requires
    // nothing", so it has to be checked before the set difference below, which cannot see it.
    if ack.required.has_bits_beyond_this_build() {
        return Err(Refusal::new(
            RefusalReason::MISSING_CAPABILITY,
            "the decoder requires a capability that is past the end of the bitset this host understands",
        ));
    }

    let missing = ack.required.difference(hello.offered);
    if let Some(cap) = missing.iter().next() {
        return Err(Refusal {
            reason: RefusalReason::MISSING_CAPABILITY,
            capability: cap,
            detail: "the decoder requires a capability this host does not offer",
        });
    }

    Ok(Agreement {
        abi_major: hello.abi_major,
        abi_minor: hello.abi_minor.min(ack.abi_minor),
        agreed: hello.offered.intersection(ack.required.union(ack.optional)),
        window_bytes: hello.window_bytes,
        max_batch_rows: hello.max_batch_rows,
    })
}

impl Agreement {
    /// Whether a capability is in force for this pairing.
    #[must_use]
    pub const fn has(&self, cap: Capability) -> bool {
        self.agreed.contains(cap)
    }
}
