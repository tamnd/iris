//! The records that make up the footer.
//!
//! Each one encodes and decodes itself against the `iris-abi` reader and writer, so the framing
//! rules are the ones already written down in `docs/ABI.md`. In particular a record may grow a
//! field at the end without a version bump, and a reader that does not know about the field steps
//! over it, which is why every `decode` here reads what it knows and stops.

use iris_abi::{CapabilitySet, Reader, Result as AbiResult, Writer};

use crate::digest::Digest;
use crate::layout::{DIGEST_SIZE, DecoderLocation, SchemaEncoding, SectionKind};

/// What the dataset is and how big it is.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Dataset {
    /// How many rows there are in total.
    ///
    /// A `u64` because a `u32` runs out at four billion rows, which is a size people already have.
    pub rows: u64,
    /// A human readable name, for error messages and for `iris describe`. Not an identifier and
    /// nothing looks it up.
    pub name: String,
}

impl Dataset {
    /// The record version this build writes.
    pub const VERSION: u16 = 1;

    /// Writes this record's payload.
    ///
    /// # Errors
    ///
    /// Returns whatever the writer returns, which in practice means the buffer was too small.
    pub fn encode(&self, w: &mut Writer<'_>) -> AbiResult<()> {
        w.u64(self.rows)?;
        w.var_str(&self.name)
    }

    /// Reads this record's payload.
    ///
    /// # Errors
    ///
    /// Returns [`iris_abi::Error`] if the payload is truncated or the name is not UTF-8.
    pub fn decode(p: &mut Reader<'_>) -> AbiResult<Self> {
        Ok(Self {
            rows: p.u64()?,
            name: p.var_str()?.to_owned(),
        })
    }
}

/// The Arrow schema, carried as bytes this crate does not look inside.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Schema<'a> {
    /// How the bytes are encoded.
    pub encoding: SchemaEncoding,
    /// The bytes.
    pub bytes: &'a [u8],
}

impl<'a> Schema<'a> {
    /// The record version this build writes.
    pub const VERSION: u16 = 1;

    /// Writes this record's payload.
    ///
    /// # Errors
    ///
    /// Returns whatever the writer returns.
    pub fn encode(&self, w: &mut Writer<'_>) -> AbiResult<()> {
        w.u32(self.encoding.code())?;
        w.var_bytes(self.bytes)
    }

    /// Reads this record's payload.
    ///
    /// # Errors
    ///
    /// Returns [`iris_abi::Error`] if the payload is truncated.
    pub fn decode(p: &mut Reader<'a>) -> AbiResult<Self> {
        Ok(Self {
            encoding: SchemaEncoding::from_code(p.u32()?),
            bytes: p.var_bytes()?,
        })
    }
}

/// Which decoder reads this dataset.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecoderRef<'a> {
    /// The ABI major version the decoder was built against.
    pub abi_major: u16,
    /// The ABI minor version the decoder was built against.
    pub abi_minor: u16,
    /// Where the module is.
    pub location: DecoderLocation,
    /// The digest of the module.
    ///
    /// This is the identity of the decoder. A host that already trusts a native implementation of
    /// this exact module substitutes it here and skips the sandbox, and a host that does not runs
    /// the bytes it hashed.
    pub digest: Digest,
    /// What the decoder needs the host to be able to do.
    ///
    /// Carrying this in the container as well as in the handshake means a host can refuse a dataset
    /// before it has loaded a single instruction of it, which is a much better error than one that
    /// arrives halfway through a query.
    pub required: CapabilitySet,
    /// A human readable name, for error messages.
    pub name: &'a str,
}

impl<'a> DecoderRef<'a> {
    /// The record version this build writes.
    pub const VERSION: u16 = 1;

    /// Writes this record's payload.
    ///
    /// # Errors
    ///
    /// Returns whatever the writer returns.
    pub fn encode(&self, w: &mut Writer<'_>) -> AbiResult<()> {
        w.u16(self.abi_major)?;
        w.u16(self.abi_minor)?;
        let (kind, section) = match self.location {
            DecoderLocation::Embedded { section } => (0, section),
            DecoderLocation::External => (1, 0),
        };
        w.u32(kind)?;
        w.u32(section)?;
        w.u32(0)?;
        w.raw(self.digest.as_bytes())?;
        w.var_bytes(self.required.as_bytes())?;
        w.var_str(self.name)
    }

    /// Reads this record's payload.
    ///
    /// An unknown location kind decodes as [`DecoderLocation::External`], because the one thing a
    /// reader must not do with a decoder it cannot place is run something else.
    ///
    /// # Errors
    ///
    /// Returns [`iris_abi::Error`] if the payload is truncated or the name is not UTF-8.
    pub fn decode(p: &mut Reader<'a>) -> AbiResult<Self> {
        let abi_major = p.u16()?;
        let abi_minor = p.u16()?;
        let kind = p.u32()?;
        let section = p.u32()?;
        let _reserved = p.u32()?;
        let raw = p.bytes(DIGEST_SIZE)?;
        let mut digest = [0u8; DIGEST_SIZE];
        digest.copy_from_slice(raw);
        Ok(Self {
            abi_major,
            abi_minor,
            location: if kind == 0 {
                DecoderLocation::Embedded { section }
            } else {
                DecoderLocation::External
            },
            digest: Digest(digest),
            required: p.capability_set()?,
            name: p.var_str()?,
        })
    }
}

/// One run of bytes in the payload area.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Section {
    /// What the rest of the container refers to this section by.
    pub id: u32,
    /// What it holds.
    pub kind: SectionKind,
    /// Where it starts, counted from the start of the file.
    pub offset: u64,
    /// How many bytes long it is.
    pub len: u64,
    /// The digest of those bytes.
    pub digest: Digest,
}

impl Section {
    /// The record version this build writes.
    pub const VERSION: u16 = 1;

    /// Where the section ends, or `None` if the arithmetic overflows.
    ///
    /// The `Option` is the point. These two numbers came out of a file somebody else wrote, and the
    /// first thing a hostile one does is pick an offset and a length that add up to something
    /// small.
    #[must_use]
    pub const fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }

    /// Writes this record's payload.
    ///
    /// # Errors
    ///
    /// Returns whatever the writer returns.
    pub fn encode(&self, w: &mut Writer<'_>) -> AbiResult<()> {
        w.u32(self.id)?;
        w.u32(self.kind.code())?;
        w.u64(self.offset)?;
        w.u64(self.len)?;
        w.raw(self.digest.as_bytes())
    }

    /// Reads this record's payload.
    ///
    /// # Errors
    ///
    /// Returns [`iris_abi::Error`] if the payload is truncated.
    pub fn decode(p: &mut Reader<'_>) -> AbiResult<Self> {
        let id = p.u32()?;
        let kind = SectionKind::from_code(p.u32()?);
        let offset = p.u64()?;
        let len = p.u64()?;
        let raw = p.bytes(DIGEST_SIZE)?;
        let mut digest = [0u8; DIGEST_SIZE];
        digest.copy_from_slice(raw);
        Ok(Self {
            id,
            kind,
            offset,
            len,
            digest: Digest(digest),
        })
    }
}
