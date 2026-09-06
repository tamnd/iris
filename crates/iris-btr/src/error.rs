//! What can go wrong reading a column part.

use crate::{ColumnType, scheme_name};

/// The result of reading something out of a column part.
pub type Result<T> = std::result::Result<T, Error>;

/// Why a column part could not be read.
///
/// Every variant here is a statement about the bytes, not about this crate. A part that came from
/// somewhere else is untrusted input, and the whole point of returning these rather than panicking
/// is that a malformed part is an answer a caller can handle.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The part ended in the middle of something it said was there.
    #[error("the part ends after {len} bytes but {what} needs bytes {from} to {to}")]
    Truncated {
        /// What was being read.
        what: &'static str,
        /// The first byte wanted.
        from: usize,
        /// One past the last byte wanted.
        to: usize,
        /// How long the part actually is.
        len: usize,
    },

    /// A chunk index past the end of the part.
    #[error("chunk {index} was asked for but the part holds {count}")]
    NoSuchChunk {
        /// The index asked for.
        index: u32,
        /// How many chunks the part holds.
        count: u32,
    },

    /// The chunk offsets do not describe a run of chunks laid out end to end.
    #[error("chunk {index} starts at {start} and the next one starts at {end}")]
    Overlapping {
        /// The chunk whose extent could not be worked out.
        index: u32,
        /// Where it starts.
        start: usize,
        /// Where the chunk after it starts.
        end: usize,
    },

    /// A column type this crate does not read.
    ///
    /// The reference names eight, and three of them carry data. The rest are placeholders in its
    /// own enum, so a part claiming one of those did not come from the reference.
    #[error("column type {code} is not one this reads")]
    UnknownColumnType {
        /// The byte in the chunk header.
        code: u8,
    },

    /// A scheme this crate does not implement yet.
    ///
    /// Separate from [`Error::UnknownScheme`] on purpose. This one says the part is fine and we are
    /// behind, which is a statement about iris, and it is the error the conformance suite counts.
    #[error("{column} scheme {} is not implemented yet", scheme_name(*column, *code))]
    UnsupportedScheme {
        /// The column type the scheme belongs to.
        column: ColumnType,
        /// The scheme code in the chunk header.
        code: u8,
    },

    /// A scheme code the reference does not define for this column type.
    #[error("{column} scheme {code} is not one the reference defines")]
    UnknownScheme {
        /// The column type the scheme was read for.
        column: ColumnType,
        /// The scheme code in the chunk header.
        code: u8,
    },

    /// A nullmap encoding the reference does not define.
    #[error("nullmap encoding {code} is not one the reference defines")]
    UnknownNullmap {
        /// The byte in the chunk header.
        code: u8,
    },

    /// A compressed stream that does not describe itself consistently.
    ///
    /// Separate from [`Error::Overrun`], which is a length that does not fit in the bytes there.
    /// This is a stream that fits and still makes no sense, which for a bit packed block means a
    /// width no packing can have or a count that is not a whole number of blocks.
    #[error("{what} is malformed: {why}")]
    Malformed {
        /// What was being read.
        what: &'static str,
        /// What was wrong with it.
        why: &'static str,
    },

    /// A length inside a chunk that does not fit in what the chunk holds.
    ///
    /// This is the field a malicious part would reach for, so it is checked against the bytes that
    /// are really there rather than used to size an allocation.
    #[error("{what} says {claimed} bytes but the chunk has {available} left")]
    Overrun {
        /// What claimed the length.
        what: &'static str,
        /// The length it claimed.
        claimed: usize,
        /// How many bytes are actually left.
        available: usize,
    },
}
