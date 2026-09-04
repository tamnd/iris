//! What can go wrong between the host and a module.

use iris_abi::{Refusal, RefusalReason};

/// A decoder module failing, or refusing.
///
/// The variants that come from the engine carry a message rather than the engine's own error type.
/// That is deliberate. This crate exists so that a Wasmtime major version is a change to one crate,
/// and putting a Wasmtime type in a public enum would make it a change to everything that matches on
/// this one.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The bytes are not a module this build can compile.
    #[error("the decoder module did not compile: {0}")]
    Compile(String),

    /// The module compiled but does not have the shape a decoder has.
    ///
    /// The detail names the export or import that was wrong, because the two ways to get here are a
    /// module built against a different version of the SDK and a module that is not a decoder at
    /// all, and those want different fixes.
    #[error("the module is not a decoder: {0}")]
    NotADecoder(String),

    /// A call into the guest trapped, ran out of its budget, or returned an error.
    #[error("the decoder trapped: {0}")]
    Trap(String),

    /// The guest handed back an address or a length that is not inside its own memory.
    ///
    /// This is the interesting failure. A guest cannot reach outside its memory, so the damage is
    /// bounded either way, but a guest that points at bytes it does not own is either broken or
    /// lying and neither is worth continuing with.
    #[error("the decoder pointed outside its own memory: {0}")]
    OutOfBounds(&'static str),

    /// A record the guest wrote does not parse.
    #[error("the decoder wrote a record that does not parse: {0}")]
    Record(#[from] iris_abi::Error),

    /// The decoder declined, and said why.
    #[error("the decoder refused: {reason}: {detail}")]
    Refused {
        /// The reason code, for a caller that wants to branch rather than print.
        reason: RefusalReason,
        /// The text, for whoever reads the log.
        detail: String,
    },
}

impl Error {
    /// Turns a refusal record into an error, copying the detail out of the guest's memory.
    #[must_use]
    pub fn refused(refusal: &Refusal<'_>) -> Self {
        Self::Refused {
            reason: refusal.reason,
            detail: refusal.detail.to_owned(),
        }
    }
}

/// What this crate returns.
pub type Result<T> = core::result::Result<T, Error>;
