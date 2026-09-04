//! What a decoder says when it cannot do what it was asked.

use core::fmt;

use iris_abi::{Capability, Refusal, RefusalReason};

/// A decoder declining to go on.
///
/// Every error a decoder can produce turns into a [`Refusal`] record on the wire, so the type
/// carries the same three things a refusal does: a reason code a host can branch on, the capability
/// involved when there was one, and a line of text for whoever has to read the log.
///
/// The detail is a `&'static str` rather than a formatted string. A decoder runs inside a sandbox
/// with a memory limit, and the moment where it has run out of something is the worst possible
/// moment to allocate in order to say so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    reason: RefusalReason,
    capability: Capability,
    detail: &'static str,
}

impl Error {
    /// A refusal with a reason and no particular capability attached.
    #[must_use]
    pub const fn new(reason: RefusalReason, detail: &'static str) -> Self {
        Self {
            reason,
            capability: Capability(u16::MAX),
            detail,
        }
    }

    /// The bytes the decoder was given do not describe what they claim to.
    #[must_use]
    pub const fn malformed(detail: &'static str) -> Self {
        Self::new(RefusalReason::MALFORMED, detail)
    }

    /// The decoder needs something the host did not offer.
    #[must_use]
    pub const fn missing(capability: Capability, detail: &'static str) -> Self {
        Self {
            reason: RefusalReason::MISSING_CAPABILITY,
            capability,
            detail,
        }
    }

    /// The request is larger than the decoder is willing to serve.
    #[must_use]
    pub const fn resource_limit(detail: &'static str) -> Self {
        Self::new(RefusalReason::RESOURCE_LIMIT, detail)
    }

    /// The decoder could do this and is choosing not to.
    #[must_use]
    pub const fn policy(detail: &'static str) -> Self {
        Self::new(RefusalReason::POLICY, detail)
    }

    /// The same thing in the other direction, for a refusal that arrived from the other side.
    #[must_use]
    pub const fn from_refusal(refusal: Refusal<'static>) -> Self {
        Self {
            reason: refusal.reason,
            capability: refusal.capability,
            detail: refusal.detail,
        }
    }

    /// Which capability was missing, when that is what went wrong.
    #[must_use]
    pub const fn capability(&self) -> Option<Capability> {
        if self.capability.0 == u16::MAX {
            None
        } else {
            Some(self.capability)
        }
    }

    /// Why the decoder stopped.
    #[must_use]
    pub const fn reason(&self) -> RefusalReason {
        self.reason
    }

    /// The text, for a human.
    #[must_use]
    pub const fn detail(&self) -> &'static str {
        self.detail
    }

    /// The same thing in the form that goes on the wire.
    #[must_use]
    pub const fn as_refusal(&self) -> Refusal<'static> {
        Refusal {
            reason: self.reason,
            capability: self.capability,
            detail: self.detail,
        }
    }
}

impl From<iris_abi::Error> for Error {
    fn from(err: iris_abi::Error) -> Self {
        // The variants collapse to two outcomes, and the distinction that survives is the one a
        // host can act on: either the bytes were wrong, or the decoder ran out of room. Which field
        // was short is in the decoder's own logs and is no use to the other side of the boundary.
        match err {
            iris_abi::Error::BufferFull { .. } | iris_abi::Error::LengthOverflow => {
                Self::resource_limit("the decoder ran out of room encoding its answer")
            }
            _ => Self::malformed("the decoder could not read a record the host sent it"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.capability.name() {
            Some(name) if self.reason == RefusalReason::MISSING_CAPABILITY => {
                write!(f, "{}: {} ({name})", self.reason, self.detail)
            }
            _ => write!(f, "{}: {}", self.reason, self.detail),
        }
    }
}

impl core::error::Error for Error {}

/// What a decoder returns.
pub type Result<T> = core::result::Result<T, Error>;
