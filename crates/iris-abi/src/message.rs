//! The records themselves.
//!
//! Each record has a fixed-width part and then a variable-length part, in that order, and new
//! fields go on the end. A reader reads what it knows and stops; the framing tells it where the
//! record ended, so stopping early is safe. That single rule is what lets a decoder built against
//! one version of this crate keep working against a host built against a later one.
//!
//! The version number on a record is not a "how new is this" counter. It only goes up when a field
//! is removed or changes meaning, which is a break, and a break is supposed to be loud.

use crate::caps::{Capability, CapabilitySet};
use crate::error::{Error, Result};
use crate::record::{Header, Tag};
use crate::wire::{Reader, Writer};

/// The host introducing itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    /// The major ABI version the host speaks. A mismatch here is fatal.
    pub abi_major: u16,
    /// The minor ABI version the host speaks. A mismatch here is not fatal.
    pub abi_minor: u16,
    /// How many bytes of the source the host is willing to keep visible to the guest at once.
    ///
    /// Zero means the host will map the whole source and the decoder never has to think about
    /// windows.
    pub window_bytes: u64,
    /// The largest number of rows the host will ask for in one scan request.
    pub max_batch_rows: u64,
    /// What the host can do.
    pub offered: CapabilitySet,
}

impl Hello {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::HELLO, Self::VERSION, |w| {
            w.u16(self.abi_major)?;
            w.u16(self.abi_minor)?;
            w.u32(0)?;
            w.u64(self.window_bytes)?;
            w.u64(self.max_batch_rows)?;
            w.var_bytes(self.offered.as_bytes())
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows, or
    /// [`Error::Truncated`] if the payload ends early.
    pub fn decode(version: u16, p: &mut Reader<'_>) -> Result<Self> {
        expect_version(Tag::HELLO, version)?;
        let abi_major = p.u16()?;
        let abi_minor = p.u16()?;
        p.skip(4)?;
        let window_bytes = p.u64()?;
        let max_batch_rows = p.u64()?;
        let offered = p.capability_set()?;
        Ok(Self {
            abi_major,
            abi_minor,
            window_bytes,
            max_batch_rows,
            offered,
        })
    }
}

/// The decoder answering the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HelloAck<'a> {
    /// The major ABI version the decoder was built against.
    pub abi_major: u16,
    /// The minor ABI version the decoder was built against.
    pub abi_minor: u16,
    /// What the decoder cannot run without. If the host does not offer all of these, the two sides
    /// stop here.
    pub required: CapabilitySet,
    /// What the decoder will use if it is there and do without if it is not.
    pub optional: CapabilitySet,
    /// A name for the decoder, for logs and error messages. Not interpreted.
    pub decoder_id: &'a str,
}

impl<'a> HelloAck<'a> {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::HELLO_ACK, Self::VERSION, |w| {
            w.u16(self.abi_major)?;
            w.u16(self.abi_minor)?;
            w.u32(0)?;
            w.var_bytes(self.required.as_bytes())?;
            w.var_bytes(self.optional.as_bytes())?;
            w.var_str(self.decoder_id)
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows,
    /// [`Error::Truncated`] if the payload ends early, or [`Error::NotUtf8`] if the decoder name is
    /// not text.
    pub fn decode(version: u16, p: &mut Reader<'a>) -> Result<Self> {
        expect_version(Tag::HELLO_ACK, version)?;
        let abi_major = p.u16()?;
        let abi_minor = p.u16()?;
        p.skip(4)?;
        let required = p.capability_set()?;
        let optional = p.capability_set()?;
        let decoder_id = p.var_str()?;
        Ok(Self {
            abi_major,
            abi_minor,
            required,
            optional,
            decoder_id,
        })
    }
}

/// Why one side is declining to go on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RefusalReason(pub u16);

impl RefusalReason {
    /// The other side needs a capability this side does not have.
    pub const MISSING_CAPABILITY: Self = Self(1);
    /// The other side speaks a major ABI version from the future.
    pub const ABI_TOO_NEW: Self = Self(2);
    /// The other side speaks a major ABI version that is no longer supported.
    pub const ABI_TOO_OLD: Self = Self(3);
    /// A record arrived that this side does not know how to handle and cannot skip.
    pub const UNSUPPORTED_RECORD: Self = Self(4);
    /// The bytes did not parse.
    pub const MALFORMED: Self = Self(5);
    /// The request is larger than this side is willing to serve.
    pub const RESOURCE_LIMIT: Self = Self(6);
    /// This side is able to do what was asked and is choosing not to.
    pub const POLICY: Self = Self(7);

    /// The name of this reason, if it is one we assigned.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::MISSING_CAPABILITY => Some("missing capability"),
            Self::ABI_TOO_NEW => Some("ABI version too new"),
            Self::ABI_TOO_OLD => Some("ABI version too old"),
            Self::UNSUPPORTED_RECORD => Some("unsupported record"),
            Self::MALFORMED => Some("malformed record"),
            Self::RESOURCE_LIMIT => Some("resource limit"),
            Self::POLICY => Some("refused by policy"),
            _ => None,
        }
    }
}

impl core::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "refusal reason {}", self.0),
        }
    }
}

/// One side declining to go on, and saying why.
///
/// The reason a refusal is a record rather than a dropped connection is that "this did not work" is
/// not an actionable message. Somebody has to be able to read the failure and know which capability
/// to go and implement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refusal<'a> {
    /// The category of problem.
    pub reason: RefusalReason,
    /// Which capability was missing, when `reason` is [`RefusalReason::MISSING_CAPABILITY`].
    pub capability: Capability,
    /// Text for a human. Not parsed by anything.
    pub detail: &'a str,
}

impl<'a> Refusal<'a> {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// A refusal with no particular capability attached.
    #[must_use]
    pub const fn new(reason: RefusalReason, detail: &'a str) -> Self {
        Self {
            reason,
            capability: Capability(u16::MAX),
            detail,
        }
    }

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::REFUSAL, Self::VERSION, |w| {
            w.u16(self.reason.0)?;
            w.u16(self.capability.0)?;
            w.u32(0)?;
            w.var_str(self.detail)
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows,
    /// [`Error::Truncated`] if the payload ends early, or [`Error::NotUtf8`] if the detail is not
    /// text.
    pub fn decode(version: u16, p: &mut Reader<'a>) -> Result<Self> {
        expect_version(Tag::REFUSAL, version)?;
        let reason = RefusalReason(p.u16()?);
        let capability = Capability(p.u16()?);
        p.skip(4)?;
        let detail = p.var_str()?;
        Ok(Self {
            reason,
            capability,
            detail,
        })
    }
}

/// The columns a scan is being asked for.
///
/// This is a list of indices and not a bitmask. A bitmask has to pick a width, and whatever width
/// it picks becomes the maximum number of columns the format can ever describe. Wide tables are
/// exactly where a columnar format is supposed to win, so putting a ceiling on the column count is
/// the wrong place to save four bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Projection<'a> {
    raw: &'a [u8],
}

impl<'a> Projection<'a> {
    /// An empty projection, which means every column.
    pub const ALL: Self = Self { raw: &[] };

    /// Wraps the encoded form.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the length is not a whole number of column indices.
    pub fn from_bytes(raw: &'a [u8]) -> Result<Self> {
        if !raw.len().is_multiple_of(4) {
            return Err(Error::Malformed(
                "a projection must be a whole number of four byte column indices",
            ));
        }
        Ok(Self { raw })
    }

    /// How many columns are named.
    #[must_use]
    pub const fn len(self) -> usize {
        self.raw.len() / 4
    }

    /// Whether the projection names no columns, which means every column.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.raw.is_empty()
    }

    /// The column indices, in the order the caller wrote them.
    pub fn iter(self) -> impl Iterator<Item = u32> + 'a {
        self.raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
    }

    /// The encoded form.
    #[must_use]
    pub const fn as_bytes(self) -> &'a [u8] {
        self.raw
    }
}

/// The host asking the decoder for a run of rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScanRequest<'a> {
    /// The first row wanted, counting from zero.
    ///
    /// This is 64 bits because a row count that fits in 32 bits is a limit somebody will hit, and
    /// the whole point of the exercise is not to build limits into a format that ossifies.
    pub row_start: u64,
    /// How many rows are wanted. `u64::MAX` means "everything from `row_start` on".
    pub row_count: u64,
    /// Flags, all currently reserved and required to be zero.
    pub flags: u64,
    /// Which columns are wanted.
    pub projection: Projection<'a>,
    /// A filter for the decoder to apply, in a form the decoder and the host have agreed on
    /// separately. Empty means no filter.
    pub filter: &'a [u8],
}

impl<'a> ScanRequest<'a> {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// A request for every row and every column.
    #[must_use]
    pub const fn everything() -> Self {
        Self {
            row_start: 0,
            row_count: u64::MAX,
            flags: 0,
            projection: Projection::ALL,
            filter: &[],
        }
    }

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::SCAN_REQUEST, Self::VERSION, |w| {
            w.u64(self.row_start)?;
            w.u64(self.row_count)?;
            w.u64(self.flags)?;
            w.var_bytes(self.projection.as_bytes())?;
            w.var_bytes(self.filter)
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows,
    /// [`Error::Truncated`] if the payload ends early, or [`Error::Malformed`] if the projection is
    /// not a whole number of column indices.
    pub fn decode(version: u16, p: &mut Reader<'a>) -> Result<Self> {
        expect_version(Tag::SCAN_REQUEST, version)?;
        let row_start = p.u64()?;
        let row_count = p.u64()?;
        let flags = p.u64()?;
        let projection = Projection::from_bytes(p.var_bytes()?)?;
        let filter = p.var_bytes()?;
        Ok(Self {
            row_start,
            row_count,
            flags,
            projection,
            filter,
        })
    }
}

/// The decoder asking for bytes of the source.
///
/// This is the record the whole design turns on. The decoder says which bytes it needs and the host
/// decides how to get them, which is what keeps file handles, caching, prefetching, object store
/// credentials and retry policy on the host side of the boundary where they can be fixed without
/// recompiling anybody's decoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RangeRequest {
    /// Where the wanted bytes start in the source.
    pub offset: u64,
    /// How many bytes are wanted.
    pub len: u64,
}

impl RangeRequest {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::RANGE_REQUEST, Self::VERSION, |w| {
            w.u64(self.offset)?;
            w.u64(self.len)
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows, or
    /// [`Error::Truncated`] if the payload ends early.
    pub fn decode(version: u16, p: &mut Reader<'_>) -> Result<Self> {
        expect_version(Tag::RANGE_REQUEST, version)?;
        let offset = p.u64()?;
        let len = p.u64()?;
        Ok(Self { offset, len })
    }
}

/// One record, decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Message<'a> {
    /// See [`Hello`].
    Hello(Hello),
    /// See [`HelloAck`].
    HelloAck(HelloAck<'a>),
    /// See [`Refusal`].
    Refusal(Refusal<'a>),
    /// See [`ScanRequest`].
    ScanRequest(ScanRequest<'a>),
    /// See [`RangeRequest`].
    RangeRequest(RangeRequest),
    /// A record this build has no code for. It has already been stepped over, so a reader that gets
    /// one of these can carry on to the next record.
    Unknown(
        /// What was on the front of the record that could not be handled.
        Header,
    ),
}

impl<'a> Reader<'a> {
    /// Reads the next record and decodes it.
    ///
    /// An unrecognised tag comes back as [`Message::Unknown`] rather than an error, and the reader
    /// is left pointing at the record after it. That is the extension point: adding a record to the
    /// ABI does not break anything that was compiled before it existed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer ends inside the record, or whatever the
    /// individual record's decoder returns.
    pub fn message(&mut self) -> Result<Message<'a>> {
        let (header, mut p) = self.record()?;
        let v = header.version;
        Ok(match header.tag {
            Tag::HELLO => Message::Hello(Hello::decode(v, &mut p)?),
            Tag::HELLO_ACK => Message::HelloAck(HelloAck::decode(v, &mut p)?),
            Tag::REFUSAL => Message::Refusal(Refusal::decode(v, &mut p)?),
            Tag::SCAN_REQUEST => Message::ScanRequest(ScanRequest::decode(v, &mut p)?),
            Tag::RANGE_REQUEST => Message::RangeRequest(RangeRequest::decode(v, &mut p)?),
            _ => Message::Unknown(header),
        })
    }
}

fn expect_version(tag: Tag, version: u16) -> Result<()> {
    // Adding a field to the end of a record does not change this number, because a reader that does
    // not know about the field skips it and is still correct. The number only moves when a field is
    // removed or changes meaning, and at that point a reader that guesses is worse than one that
    // stops. A future version 2 of a record keeps a branch here for version 1.
    let known = match tag {
        Tag::HELLO => Hello::VERSION,
        Tag::HELLO_ACK => HelloAck::VERSION,
        Tag::REFUSAL => Refusal::VERSION,
        Tag::SCAN_REQUEST => ScanRequest::VERSION,
        Tag::RANGE_REQUEST => RangeRequest::VERSION,
        _ => return Err(Error::Malformed("no version is defined for this tag")),
    };
    if version == known {
        Ok(())
    } else {
        Err(Error::UnsupportedVersion { tag, version })
    }
}
