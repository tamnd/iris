//! Where everything sits in the file.
//!
//! ```text
//! +--------------------------------------------------+ 0
//! | header, 16 bytes                                 |
//! +--------------------------------------------------+ 16
//! | sections, back to back, each 8 byte aligned      |
//! +--------------------------------------------------+ footer_offset
//! | footer, a run of iris-abi records                |
//! +--------------------------------------------------+ len - 56
//! | trailer, 56 bytes                                |
//! +--------------------------------------------------+ len
//! ```
//!
//! The directory lives at the end rather than the front, because a writer that streams a large
//! dataset does not know how long a section is until it has finished writing it, and the
//! alternative is either buffering the whole thing or seeking back over it. The magic appears at
//! both ends so that a file truncated in the middle is distinguishable from a file that was never
//! an iris container in the first place.

use iris_abi::Tag;

/// The first eight bytes of every container, and the last eight.
///
/// The carriage return and line feed catch a transport that helpfully converts line endings, and
/// the `0x1a` stops a Windows `type` from printing the rest of the file. Both of those are older
/// than most of the people who will read this and both still happen.
pub const MAGIC: [u8; 8] = *b"IRIS\r\n\x1a\n";

/// The format major version this build writes and reads.
///
/// A major version goes up when a reader that does not know about the change would get the wrong
/// answer rather than an error.
pub const FORMAT_MAJOR: u16 = 0;

/// The format minor version this build writes.
///
/// A minor version goes up when something is added that an older reader can safely ignore, which in
/// practice means a new footer record or a new field at the end of an existing one.
pub const FORMAT_MINOR: u16 = 1;

/// How wide the header is, in bytes.
pub const HEADER_SIZE: usize = 16;

/// How wide the trailer is, in bytes.
pub const TRAILER_SIZE: usize = 56;

/// The smallest a container can possibly be, which is a header, an empty footer and a trailer.
pub const MIN_SIZE: usize = HEADER_SIZE + TRAILER_SIZE;

/// How wide a digest is, in bytes.
pub const DIGEST_SIZE: usize = 32;

/// The footer record tags.
///
/// These share the `Tag` newtype with the call protocol in `iris-abi` but not its number space. A
/// footer record and a call record never appear in the same byte stream, so there is nothing to
/// collide, and reusing the framing means the skip an unknown record rule is the one that is
/// already written down and already tested.
pub mod tag {
    use super::Tag;

    /// How many rows the dataset has, and what it is called.
    pub const DATASET: Tag = Tag(0x0100);
    /// The Arrow schema, as opaque bytes plus an encoding.
    pub const SCHEMA: Tag = Tag(0x0101);
    /// Which decoder reads this dataset, and what it needs from the host.
    pub const DECODER: Tag = Tag(0x0102);
    /// One run of bytes in the payload area, with its digest.
    pub const SECTION: Tag = Tag(0x0103);
}

/// How the schema bytes are encoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SchemaEncoding {
    /// An Arrow IPC schema message, exactly as `arrow-ipc` writes one.
    ///
    /// This crate does not depend on Arrow and does not look inside these bytes. Carrying the
    /// encoding rather than assuming it means a later version can add a second one without a new
    /// major version, and it means this crate stays small enough to fuzz properly.
    ArrowIpc,
    /// Something this build does not know about, carried through so that a tool which only wants
    /// the section table is not stopped by a schema it cannot read.
    Unknown(u32),
}

impl SchemaEncoding {
    /// The number this encoding is written as.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::ArrowIpc => 1,
            Self::Unknown(code) => code,
        }
    }

    /// The encoding a code stands for.
    #[must_use]
    pub const fn from_code(code: u32) -> Self {
        match code {
            1 => Self::ArrowIpc,
            other => Self::Unknown(other),
        }
    }
}

/// What a section holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SectionKind {
    /// Encoded column data, in whatever form the decoder expects.
    Data,
    /// A decoder module, embedded in the container.
    Decoder,
    /// An index, a dictionary, a footer of some inner format, anything the decoder wants to find
    /// quickly without reading the data.
    Sidecar,
    /// A kind this build does not know about. A section is a run of bytes with a digest, so an
    /// unknown kind is still checkable and still skippable.
    Unknown(u32),
}

impl SectionKind {
    /// The number this kind is written as.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Data => 1,
            Self::Decoder => 2,
            Self::Sidecar => 3,
            Self::Unknown(code) => code,
        }
    }

    /// The kind a code stands for.
    #[must_use]
    pub const fn from_code(code: u32) -> Self {
        match code {
            1 => Self::Data,
            2 => Self::Decoder,
            3 => Self::Sidecar,
            other => Self::Unknown(other),
        }
    }
}

/// Where the decoder module for a dataset lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DecoderLocation {
    /// In a section of this container, named by its section id.
    ///
    /// This is the self-decoding case and the one the project is about. The dataset carries the
    /// code that reads it, so a host that has never seen the encoding can still read the data.
    Embedded {
        /// Which section holds the module.
        section: u32,
    },
    /// Somewhere else, to be resolved by digest.
    ///
    /// Useful when many datasets share one decoder and nobody wants a copy of it in every file.
    /// The digest is what makes this safe: the host either finds bytes that hash to it or it does
    /// not run anything.
    External,
}
