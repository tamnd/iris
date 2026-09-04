//! What it means for a decoder to be untrusted.

use iris_format::Digest;

/// Why a decoder was not handed over.
///
/// Only one of these is a security event. The other three say the container does not carry a module
/// this host can run, which is a bad file or an unfinished feature, and a host that wants to tell
/// those apart in a log can match on the variant rather than read the text.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Untrusted {
    /// The container does not say which decoder reads it.
    ///
    /// A container without a decoder reference is not unreadable, it is just not self decoding, and
    /// reading it is somebody else's problem rather than this crate's.
    #[error("the container names no decoder, so nothing here knows how to read it")]
    Missing,

    /// The decoder lives somewhere else and this crate cannot go and get it.
    ///
    /// Resolving one means a registry, a cache and a policy about what is allowed to be fetched,
    /// and none of those exist yet. The digest is in the container either way, so whatever ends up
    /// doing the fetching has something to check the bytes against.
    #[error(
        "the decoder for this dataset lives outside the container, which this host cannot resolve \
         yet"
    )]
    External,

    /// The decoder reference names a section the file does not have.
    ///
    /// The footer parsed and then disagreed with itself. Nothing was substituted for the missing
    /// module and nothing ever will be, because a module that is not there has no digest to check.
    #[error(
        "the container puts its decoder in section {section}, and there is no section {section} \
         in the file"
    )]
    Lost {
        /// The section id the decoder reference names.
        section: u32,
    },

    /// The module in the container does not hash to what the container says it should.
    ///
    /// Both digests are in the message on purpose. The expected one identifies the decoder that was
    /// meant to be here, which is the thing to go and look for, and the found one identifies what
    /// actually arrived, which is the thing to keep for whoever asks how it got there.
    #[error("the decoder module hashes to {found}, and the container says it should be {expected}")]
    Digest {
        /// What the container claims the module is.
        expected: Digest,
        /// What the bytes in the container actually hash to.
        found: Digest,
    },
}
