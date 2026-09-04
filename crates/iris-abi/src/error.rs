//! Errors from reading and writing ABI records.
//!
//! There is no `thiserror` here, and there will not be. This crate ends up inside every decoder
//! anyone writes, so it carries no dependencies at all and the boilerplate is written out by hand.

use core::fmt;

use crate::record::Tag;

/// What went wrong while reading or writing an ABI record.
///
/// Every variant is a fact about the bytes rather than an opinion about them. Deciding what to do
/// about a malformed record is the caller's job, because the host and the guest have very different
/// options available to them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The buffer ended before the value did.
    Truncated {
        /// How many bytes the read wanted.
        needed: usize,
        /// How many bytes were left.
        available: usize,
    },
    /// There was not enough room left in the output buffer.
    BufferFull {
        /// How many bytes the write wanted.
        needed: usize,
        /// How many bytes were left.
        available: usize,
    },
    /// A length field described more bytes than can be addressed on this target.
    LengthOverflow,
    /// A field that is declared to be text was not valid UTF-8.
    NotUtf8,
    /// The bytes parsed but do not describe a record this code can make sense of.
    Malformed(
        /// What specifically was wrong. This is a fixed string rather than a formatted one so that
        /// the crate stays free of allocation.
        &'static str,
    ),
    /// The record is one we know, at a version we do not.
    ///
    /// This is not the same as an unknown tag. An unknown tag is skipped, because that is what
    /// makes the format extensible. A known tag at an unknown version is a real disagreement and
    /// the reader has to say so.
    UnsupportedVersion {
        /// Which record.
        tag: Tag,
        /// The version that was on the wire.
        version: u16,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => write!(
                f,
                "record is truncated: wanted {needed} bytes, {available} available"
            ),
            Self::BufferFull { needed, available } => write!(
                f,
                "output buffer is full: wanted {needed} bytes, {available} available"
            ),
            Self::LengthOverflow => f.write_str("a length field does not fit in a pointer"),
            Self::NotUtf8 => f.write_str("a text field is not valid UTF-8"),
            Self::Malformed(what) => write!(f, "malformed record: {what}"),
            Self::UnsupportedVersion { tag, version } => write!(
                f,
                "record {tag} is at version {version}, which this build does not understand"
            ),
        }
    }
}

impl core::error::Error for Error {}

/// The result of an ABI read or write.
pub type Result<T> = core::result::Result<T, Error>;
