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
    /// How many bytes the source has in total, or zero if the host is not saying.
    ///
    /// A decoder that has to find its own footer needs this, and a decoder that reads forwards from
    /// the start does not, which is why zero is allowed rather than being an error.
    ///
    /// This field was appended after the ABI shipped, so it is the first real exercise of the
    /// grow-at-the-end rule. A host built before it existed writes a `Hello` that ends after
    /// `offered`, and a decoder built after it reads zero and carries on.
    pub source_bytes: u64,
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
            w.var_bytes(self.offered.as_bytes())?;
            w.u64(self.source_bytes)
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
        let source_bytes = p.opt_u64()?.unwrap_or(0);
        Ok(Self {
            abi_major,
            abi_minor,
            window_bytes,
            max_batch_rows,
            offered,
            source_bytes,
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

/// One array in a batch, in the flattened order the schema puts them in.
///
/// This is the same pair Arrow IPC calls a field node, and it is here for the same reason: a
/// column's length and null count are not derivable from its buffers, so somebody has to say them
/// out loud.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Node {
    /// How many slots the array has.
    pub length: u64,
    /// How many of those slots are null.
    pub null_count: u64,
}

impl Node {
    /// How wide the encoded form is, in bytes.
    pub const SIZE: usize = 16;
}

/// Where one Arrow buffer sits in the decoder's memory.
///
/// Offsets are 64 bits wide even though a `wasm32` guest cannot address more than four gigabytes,
/// because the width of a guest address is not something worth writing into a format that ossifies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferRef {
    /// Where the buffer starts in the decoder's memory.
    pub offset: u64,
    /// How many bytes long it is.
    pub len: u64,
}

impl BufferRef {
    /// How wide the encoded form is, in bytes.
    pub const SIZE: usize = 16;

    /// One past the last byte, or `None` if the two fields overflow.
    #[must_use]
    pub const fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }
}

/// A list of [`Node`]s, still in its encoded form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Nodes<'a> {
    raw: &'a [u8],
}

impl<'a> Nodes<'a> {
    /// No arrays at all.
    pub const EMPTY: Self = Self { raw: &[] };

    /// Wraps the encoded form.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the length is not a whole number of nodes.
    pub fn from_bytes(raw: &'a [u8]) -> Result<Self> {
        if !raw.len().is_multiple_of(Node::SIZE) {
            return Err(Error::Malformed(
                "a node list must be a whole number of sixteen byte nodes",
            ));
        }
        Ok(Self { raw })
    }

    /// How many arrays are described.
    #[must_use]
    pub const fn len(self) -> usize {
        self.raw.len() / Node::SIZE
    }

    /// Whether no arrays are described.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.raw.is_empty()
    }

    /// The nodes, in order.
    pub fn iter(self) -> impl Iterator<Item = Node> + 'a {
        self.raw.as_chunks::<{ Node::SIZE }>().0.iter().map(|c| {
            let (length, null_count) = c.split_at(8);
            Node {
                length: u64::from_le_bytes(length.try_into().unwrap_or([0; 8])),
                null_count: u64::from_le_bytes(null_count.try_into().unwrap_or([0; 8])),
            }
        })
    }

    /// The encoded form.
    #[must_use]
    pub const fn as_bytes(self) -> &'a [u8] {
        self.raw
    }
}

/// A list of [`BufferRef`]s, still in its encoded form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Buffers<'a> {
    raw: &'a [u8],
}

impl<'a> Buffers<'a> {
    /// No buffers at all.
    pub const EMPTY: Self = Self { raw: &[] };

    /// Wraps the encoded form.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] if the length is not a whole number of buffer references.
    pub fn from_bytes(raw: &'a [u8]) -> Result<Self> {
        if !raw.len().is_multiple_of(BufferRef::SIZE) {
            return Err(Error::Malformed(
                "a buffer list must be a whole number of sixteen byte references",
            ));
        }
        Ok(Self { raw })
    }

    /// How many buffers are described.
    #[must_use]
    pub const fn len(self) -> usize {
        self.raw.len() / BufferRef::SIZE
    }

    /// Whether no buffers are described.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.raw.is_empty()
    }

    /// The buffer references, in order.
    pub fn iter(self) -> impl Iterator<Item = BufferRef> + 'a {
        self.raw
            .as_chunks::<{ BufferRef::SIZE }>()
            .0
            .iter()
            .map(|c| {
                let (offset, len) = c.split_at(8);
                BufferRef {
                    offset: u64::from_le_bytes(offset.try_into().unwrap_or([0; 8])),
                    len: u64::from_le_bytes(len.try_into().unwrap_or([0; 8])),
                }
            })
    }

    /// The encoded form.
    #[must_use]
    pub const fn as_bytes(self) -> &'a [u8] {
        self.raw
    }
}

/// The decoder handing back one batch of decoded rows.
///
/// A batch says how many rows it has and then describes the Arrow arrays behind them as a flat list
/// of nodes and a flat list of buffers, both in the pre-order the schema puts its fields in. That is
/// the same shape Arrow IPC uses, and it is the right one here for the same reason: the host already
/// has the schema, so the schema decides how many nodes and buffers there should be and the batch
/// only has to supply them.
///
/// Neither list carries a count. The record length bounds both of them, so a batch cannot claim a
/// million columns without being large enough to describe a million columns. That is the same rule
/// the container format follows, and it is the difference between allocation safety being structural
/// and allocation safety being something a reviewer has to remember.
///
/// The buffers are not in this record. They are in the decoder's memory, and the offsets say where.
/// Whether those offsets are inside the decoder's memory, and whether the bytes at them are a valid
/// Arrow array, are two separate questions and neither of them is answered here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Batch<'a> {
    /// How many rows the batch has.
    pub rows: u64,
    /// Flags, all currently reserved and required to be zero.
    pub flags: u64,
    /// One node per array, in schema pre-order.
    pub nodes: Nodes<'a>,
    /// One reference per Arrow buffer, in schema pre-order.
    pub buffers: Buffers<'a>,
}

impl<'a> Batch<'a> {
    /// The layout version of this record.
    pub const VERSION: u16 = 1;

    /// An empty batch, which is how a decoder says a scan produced no more rows.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            rows: 0,
            flags: 0,
            nodes: Nodes::EMPTY,
            buffers: Buffers::EMPTY,
        }
    }

    /// Writes the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if the buffer runs out.
    pub fn encode(&self, w: &mut Writer<'_>) -> Result<()> {
        w.record(Tag::BATCH, Self::VERSION, |w| {
            w.u64(self.rows)?;
            w.u64(self.flags)?;
            w.var_bytes(self.nodes.as_bytes())?;
            w.var_bytes(self.buffers.as_bytes())
        })
    }

    /// Reads the record from its payload.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the layout version is not one this build knows,
    /// [`Error::Truncated`] if the payload ends early, or [`Error::Malformed`] if either list is not
    /// a whole number of entries.
    pub fn decode(version: u16, p: &mut Reader<'a>) -> Result<Self> {
        expect_version(Tag::BATCH, version)?;
        let rows = p.u64()?;
        let flags = p.u64()?;
        let nodes = Nodes::from_bytes(p.var_bytes()?)?;
        let buffers = Buffers::from_bytes(p.var_bytes()?)?;
        Ok(Self {
            rows,
            flags,
            nodes,
            buffers,
        })
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
    /// See [`Batch`].
    Batch(Batch<'a>),
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
            Tag::BATCH => Message::Batch(Batch::decode(v, &mut p)?),
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
        Tag::BATCH => Batch::VERSION,
        _ => return Err(Error::Malformed("no version is defined for this tag")),
    };
    if version == known {
        Ok(())
    } else {
        Err(Error::UnsupportedVersion { tag, version })
    }
}
