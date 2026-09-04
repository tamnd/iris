//! What can go wrong between a container and a `RecordBatch`.

use iris_abi::{Refusal, RefusalReason};

/// A dataset that cannot be opened, a decoder that refused, or a batch that does not match its
/// schema.
///
/// The three sources are kept apart on purpose. A container problem is a bad file, a decoder
/// problem is bad code, and a shape problem is the two disagreeing, and those go to three different
/// people.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The container did not parse, or a digest did not match.
    #[error(transparent)]
    Container(#[from] iris_format::Error),

    /// The decoder module did not compile, did not run, or refused.
    #[error(transparent)]
    Vm(#[from] iris_vm::Error),

    /// A record this host had to write or read did not encode.
    #[error(transparent)]
    Record(#[from] iris_abi::Error),

    /// Arrow said no.
    ///
    /// Reaching this after the guard has passed a batch means the guard and Arrow disagree about
    /// what a sound array is, which is a bug in one of them rather than a bad dataset.
    #[error(transparent)]
    Arrow(#[from] arrow_schema::ArrowError),

    /// The guard refused an array, or a schema.
    ///
    /// This is the one error in this enum that says the decoder is hostile rather than merely
    /// wrong. It carries the rule that was broken as a value, so a host can count refusals by kind
    /// and alert on the kinds that do not happen by accident.
    #[error(transparent)]
    Guard(#[from] iris_guard::Violation),

    /// The container does not say which decoder reads it.
    ///
    /// A container without a decoder reference is not unreadable, it is just not self decoding, and
    /// reading it is somebody else's problem rather than this crate's.
    #[error("the container names no decoder, so nothing here knows how to read it")]
    NoDecoder,

    /// The decoder lives somewhere else and this crate cannot go and get it.
    ///
    /// Resolving an external decoder means a registry, a cache and a policy about what is allowed
    /// to be fetched, and none of those exist yet. The digest is in the container either way, so
    /// whatever ends up doing the fetching has something to check the bytes against.
    #[error(
        "the decoder for this dataset lives outside the container, which this host cannot resolve yet"
    )]
    ExternalDecoder,

    /// The module in the container does not hash to what the container says it should.
    ///
    /// This is the check that has to happen before the module is compiled rather than after,
    /// because compiling is the first thing that treats those bytes as code.
    #[error("the decoder module hashes to {found}, and the container says it should be {expected}")]
    DecoderDigest {
        /// What the footer claims.
        expected: String,
        /// What the bytes actually hash to.
        found: String,
    },

    /// The container has no schema, or one in an encoding this build does not read.
    #[error("the container's schema is {0}, and this host reads Arrow IPC")]
    SchemaEncoding(String),

    /// The decoder was built against a major ABI version this host does not speak.
    ///
    /// The message carries the ABI version, the decoder digest and the schema because those are the
    /// three things somebody holding an unreadable dataset needs. The version says which host would
    /// read it, the digest identifies the exact decoder to go and find, and the schema says whether
    /// the dataset is even the one they were looking for. A parse error says none of that and sends
    /// them reading hex dumps instead.
    #[error(
        "this dataset needs iris ABI {needed_major}.{needed_minor} and this host speaks \
         {host_major}.{host_minor}, so its decoder cannot run here. The decoder is named {name}, \
         its module digest is {digest}, and its schema is {schema}."
    )]
    Abi {
        /// The major version the container asks for.
        needed_major: u16,
        /// The minor version the container asks for.
        needed_minor: u16,
        /// The major version this build speaks.
        host_major: u16,
        /// The minor version this build speaks.
        host_minor: u16,
        /// The decoder's human readable name, as the container gives it.
        name: String,
        /// The digest of the decoder module, which is its identity.
        digest: String,
        /// The columns, so a reader can tell whether this is the dataset they wanted.
        schema: String,
    },

    /// The container does not hold exactly one data section.
    ///
    /// M1 hands the decoder one run of bytes and calls it the source. A dataset split across
    /// several sections is a real thing and reading one is M4 work, where the host stops handing
    /// over the whole source at once anyway.
    #[error("this host reads a container with one data section, and this one has {0}")]
    DataSections(usize),

    /// The host and the decoder could not agree on terms.
    #[error("the decoder and this host could not agree: {reason}: {detail}")]
    Refused {
        /// The reason code, for a caller that wants to branch rather than print.
        reason: RefusalReason,
        /// The text, for whoever reads the log.
        detail: String,
    },

    /// A batch does not describe the arrays its schema says it should.
    ///
    /// Every way to get here is a decoder bug, and the detail says which count was wrong, because
    /// "the batch is wrong" is not something anybody can act on.
    #[error("a batch does not match the schema: {0}")]
    Shape(String),
}

impl Error {
    /// A batch that disagrees with its schema.
    pub(crate) fn shape(detail: impl Into<String>) -> Self {
        Self::Shape(detail.into())
    }

    /// Turns a refusal this host produced into an error.
    pub(crate) fn refused(refusal: &Refusal<'_>) -> Self {
        Self::Refused {
            reason: refusal.reason,
            detail: refusal.detail.to_owned(),
        }
    }
}

/// What this crate returns.
pub type Result<T> = core::result::Result<T, Error>;
