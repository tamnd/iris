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

    /// The decoder lives somewhere else and this host was not told it may go and get it.
    ///
    /// This is the default and it fails closed. A decoder named by a URI means a dataset can cause
    /// a fetch and then have the result executed, which may well be fine and is not something a
    /// host should end up doing because nobody thought about it.
    #[error(
        "the decoder named {name} for this dataset lives outside the container, and this host runs \
         embedded decoders only. A host that means to run this one calls \
         Policy::with_external_decoders_resolved_by and supplies the bytes, which are hashed \
         against the digest the container gives either way."
    )]
    External {
        /// The decoder's name, as the container gives it.
        name: String,
    },

    /// The decoder lives somewhere else, this host was told it may go and get it, and it came back
    /// with nothing.
    ///
    /// A resolver that cannot find a decoder is an ordinary outcome rather than an attack: the
    /// registry is down, or the module was never published, or this host has no copy. The digest
    /// is here because it is what the next host to try should look for.
    #[error("nothing resolved the decoder named {name}, whose module should hash to {digest}")]
    Unresolved {
        /// The decoder's name, as the container gives it.
        name: String,
        /// The digest the module has to hash to, whoever finds it.
        digest: Digest,
    },

    /// The container puts the decoder somewhere this build has never heard of.
    ///
    /// A newer writer describing a location this build does not know about is a file from the
    /// future, and the only safe reading of one is that this host cannot read it. Guessing which of
    /// the locations it does know about was meant is how a host ends up running the wrong bytes.
    #[error(
        "the container puts the decoder named {name} somewhere this build has no idea how to \
         reach, so nothing is run"
    )]
    Elsewhere {
        /// The decoder's name, as the container gives it.
        name: String,
    },

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
