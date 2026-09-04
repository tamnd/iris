//! Record framing.
//!
//! Every message that crosses the boundary is a tagged, versioned, length-prefixed record. The
//! length is what makes the format extensible: a reader that does not recognise a tag steps over
//! the record instead of giving up, and a reader that recognises a tag but was compiled before some
//! of its fields existed reads the fields it knows and steps over the rest.
//!
//! That is the whole compatibility story, and it is deliberately small enough to hold in your head.
//! The rules are written out in `docs/ABI.md` and the tests in `tests/forward_compat.rs` are there
//! to stop anybody quietly breaking them.

use core::fmt;

use crate::error::{Error, Result};
use crate::wire::{Reader, Writer, align_up};

/// Which kind of record this is.
///
/// This is a newtype over `u16` rather than an enum on purpose. An enum would make an unknown tag
/// unrepresentable, and being able to represent an unknown tag is exactly what a reader needs in
/// order to skip one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Tag(pub u16);

impl Tag {
    /// The host introducing itself and saying what it can do.
    pub const HELLO: Self = Self(0x0001);
    /// The decoder answering, and saying what it needs.
    pub const HELLO_ACK: Self = Self(0x0002);
    /// Either side declining to go on, with a reason.
    pub const REFUSAL: Self = Self(0x0003);
    /// The host asking the decoder for a run of rows.
    pub const SCAN_REQUEST: Self = Self(0x0010);
    /// The decoder asking for bytes of the source it has not been given yet.
    pub const RANGE_REQUEST: Self = Self(0x0020);
    /// The decoder handing back one batch of decoded rows.
    pub const BATCH: Self = Self(0x0030);

    /// The first tag in the range reserved for private extensions.
    ///
    /// Nothing in this range will ever be assigned a meaning by us, so anybody can use it for their
    /// own records without having to worry about a future version of iris colliding with them.
    pub const EXPERIMENTAL_BASE: Self = Self(0xFF00);

    /// Whether this tag is in the private extension range.
    #[must_use]
    pub const fn is_experimental(self) -> bool {
        self.0 >= Self::EXPERIMENTAL_BASE.0
    }

    /// The name of this tag, if it is one we assigned.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::HELLO => Some("Hello"),
            Self::HELLO_ACK => Some("HelloAck"),
            Self::REFUSAL => Some("Refusal"),
            Self::SCAN_REQUEST => Some("ScanRequest"),
            Self::RANGE_REQUEST => Some("RangeRequest"),
            Self::BATCH => Some("Batch"),
            _ => None,
        }
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name} (0x{:04x})", self.0),
            None if self.is_experimental() => write!(f, "experimental 0x{:04x}", self.0),
            None => write!(f, "unknown 0x{:04x}", self.0),
        }
    }
}

/// The eight bytes in front of every record payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Which record this is.
    pub tag: Tag,
    /// Which version of that record's layout the payload uses.
    ///
    /// A version only goes up when a field is removed or changes meaning. Adding a field at the end
    /// does not need a new version, because a reader that does not know about the field skips it.
    pub version: u16,
    /// How many bytes of payload follow, not counting the padding after them.
    pub len: u32,
}

impl Header {
    /// How wide a header is, in bytes.
    pub const SIZE: usize = 8;
}

impl<'a> Reader<'a> {
    /// Reads a record header and returns a reader over just that record's payload.
    ///
    /// The outer reader is left pointing at the next record, past the payload and its padding, so a
    /// caller that does not care about this record can simply drop the payload reader.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the header or the payload runs off the end, or
    /// [`Error::LengthOverflow`] if the declared length does not fit in a `usize`.
    pub fn record(&mut self) -> Result<(Header, Reader<'a>)> {
        let tag = Tag(self.u16()?);
        let version = self.u16()?;
        let len = self.u32()?;
        let payload_len = usize::try_from(len).map_err(|_| Error::LengthOverflow)?;
        let padded = align_up(payload_len);
        let mut body = self.sub(padded.min(self.remaining()))?;
        if body.remaining() < payload_len {
            return Err(Error::Truncated {
                needed: payload_len,
                available: body.remaining(),
            });
        }
        let payload = body.sub(payload_len)?;
        Ok((Header { tag, version, len }, payload))
    }
}

impl Writer<'_> {
    /// Writes a record, filling in its length once the body has been written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out, [`Error::LengthOverflow`] if the body
    /// is longer than a `u32` can describe, or whatever the body itself returns.
    pub fn record(
        &mut self,
        tag: Tag,
        version: u16,
        body: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.u16(tag.0)?;
        self.u16(version)?;
        let len_at = self.position();
        self.u32(0)?;
        let start = self.position();
        body(self)?;
        let len = u32::try_from(self.position() - start).map_err(|_| Error::LengthOverflow)?;
        self.patch_u32(len_at, len)?;
        self.align()
    }
}
