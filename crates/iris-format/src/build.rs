//! Writing a container.
//!
//! The builder holds its sections in memory and can either hand back the finished file or write it
//! out as it goes. Writing it out is what makes a dataset larger than this host's address space
//! possible to produce at all, and it is possible because the format puts its directory at the end:
//! nothing written early depends on anything decided late.
//!
//! What is left is that the sections themselves are still held, so a four gigabyte section costs
//! four gigabytes here. Fixing that means letting a caller push bytes into a section rather than
//! hand one over whole, and the layout already allows it.

use iris_abi::{CapabilitySet, Writer, wire};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::layout::{
    DecoderLocation, FORMAT_MAJOR, FORMAT_MINOR, HEADER_SIZE, MAGIC, SchemaEncoding, SectionKind,
    TRAILER_SIZE, tag,
};
use crate::meta::{Dataset, DecoderRef, Schema, Section};

/// A decoder reference before it has been written, holding its own name.
#[derive(Clone, Debug)]
struct PendingDecoder {
    abi_major: u16,
    abi_minor: u16,
    location: DecoderLocation,
    digest: Digest,
    required: CapabilitySet,
    name: String,
}

/// Builds a container.
///
/// ```
/// use iris_format::{Builder, Container, SectionKind};
///
/// let mut builder = Builder::new("readings", 3);
/// let data = builder.section(SectionKind::Data, b"three rows go here".to_vec());
/// let bytes = builder.build()?;
///
/// let container = Container::parse(&bytes)?;
/// container.verify()?;
/// assert_eq!(container.dataset().rows, 3);
/// assert_eq!(
///     container.section_bytes(container.section(data).unwrap()),
///     b"three rows go here"
/// );
/// # Ok::<(), iris_format::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Builder {
    dataset: Dataset,
    schema: Option<(SchemaEncoding, Vec<u8>)>,
    decoder: Option<PendingDecoder>,
    sections: Vec<(u32, SectionKind, Vec<u8>)>,
    next_id: u32,
}

impl Builder {
    /// Starts a container for a dataset with this name and this many rows.
    #[must_use]
    pub fn new(name: impl Into<String>, rows: u64) -> Self {
        Self {
            dataset: Dataset {
                rows,
                name: name.into(),
            },
            schema: None,
            decoder: None,
            sections: Vec::new(),
            next_id: 0,
        }
    }

    /// Sets the schema.
    pub fn schema(&mut self, encoding: SchemaEncoding, bytes: impl Into<Vec<u8>>) -> &mut Self {
        self.schema = Some((encoding, bytes.into()));
        self
    }

    /// Adds a section and returns the id the rest of the container refers to it by.
    pub fn section(&mut self, kind: SectionKind, bytes: impl Into<Vec<u8>>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.sections.push((id, kind, bytes.into()));
        id
    }

    /// Puts a decoder module in the container and points the dataset at it.
    ///
    /// Returns the id of the section the module went into, which is worth having for a tool that
    /// wants to print the layout.
    pub fn embed_decoder(
        &mut self,
        name: impl Into<String>,
        abi: (u16, u16),
        required: CapabilitySet,
        module: impl Into<Vec<u8>>,
    ) -> u32 {
        let module = module.into();
        let digest = Digest::of(&module);
        let section = self.section(SectionKind::Decoder, module);
        self.decoder = Some(PendingDecoder {
            abi_major: abi.0,
            abi_minor: abi.1,
            location: DecoderLocation::Embedded { section },
            digest,
            required,
            name: name.into(),
        });
        section
    }

    /// Points the dataset at a decoder that lives somewhere else, named by its digest.
    pub fn external_decoder(
        &mut self,
        name: impl Into<String>,
        abi: (u16, u16),
        required: CapabilitySet,
        digest: Digest,
    ) -> &mut Self {
        self.decoder = Some(PendingDecoder {
            abi_major: abi.0,
            abi_minor: abi.1,
            location: DecoderLocation::External,
            digest,
            required,
            name: name.into(),
        });
        self
    }

    /// Lays the container out and returns the bytes.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Footer`] if a name or a schema is longer than the wire format can
    /// describe, which takes four gigabytes of it.
    pub fn build(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.build_into(&mut out)?;
        Ok(out)
    }

    /// Lays the container out and writes it, returning how many bytes that was.
    ///
    /// The same file as [`Builder::build`], written rather than collected. It matters when the
    /// sections are large: a container written this way costs the sections themselves and about a
    /// kilobyte, where collecting it costs the sections twice over and briefly three times while a
    /// buffer grows. A four gigabyte dataset is the difference between a machine that can write one
    /// and a machine that cannot.
    ///
    /// The sections still have to be in memory, because this builder holds them. That is the next
    /// thing to fix and the layout is already arranged for it: the directory is at the end, so
    /// nothing written earlier depends on anything decided later.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] if the writer refused anything, and
    /// [`crate::Error::Footer`] if a name or a schema is longer than the wire format can describe.
    pub fn build_into(&self, out: impl std::io::Write) -> Result<u64> {
        let mut out = Counted { inner: out, at: 0 };

        out.write(&MAGIC)?;
        out.write(&FORMAT_MAJOR.to_le_bytes())?;
        out.write(&FORMAT_MINOR.to_le_bytes())?;
        out.write(&0u32.to_le_bytes())?;
        debug_assert_eq!(out.at, HEADER_SIZE as u64);

        let mut placed = Vec::with_capacity(self.sections.len());
        for (id, kind, bytes) in &self.sections {
            out.pad()?;
            placed.push(Section {
                id: *id,
                kind: *kind,
                offset: out.at,
                len: bytes.len() as u64,
                digest: Digest::of(bytes),
            });
            out.write(bytes)?;
        }

        out.pad()?;
        let footer_offset = out.at;
        let footer = self.encode_footer(&placed)?;
        out.write(&footer)?;

        // The header is rebuilt here rather than kept, because it is sixteen bytes of constants and
        // holding the bytes that were written would be one more thing that has to stay in step with
        // what was written.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&MAGIC);
        hasher.update(&FORMAT_MAJOR.to_le_bytes());
        hasher.update(&FORMAT_MINOR.to_le_bytes());
        hasher.update(&0u32.to_le_bytes());
        hasher.update(&footer);
        let root = Digest(*hasher.finalize().as_bytes());

        out.write(&footer_offset.to_le_bytes())?;
        // The footer is metadata about a dataset, not the dataset. Four gigabytes of it would mean
        // something has gone wrong that a wider length field would not fix.
        let footer_len =
            u32::try_from(footer.len()).map_err(|_| iris_abi::Error::LengthOverflow)?;
        out.write(&footer_len.to_le_bytes())?;
        out.write(&0u32.to_le_bytes())?;
        out.write(root.as_bytes())?;
        out.write(&MAGIC)?;
        debug_assert_eq!(
            out.at,
            footer_offset + footer.len() as u64 + TRAILER_SIZE as u64
        );

        Ok(out.at)
    }

    /// Encodes the footer, growing the buffer until it fits.
    ///
    /// The `iris-abi` writer never grows its own buffer, which is what makes it usable from a guest
    /// with no allocator. On the host side the cost of guessing wrong is one memcpy of a footer, so
    /// guessing and retrying is simpler than computing the exact size twice and keeping the two
    /// computations in agreement.
    fn encode_footer(&self, sections: &[Section]) -> Result<Vec<u8>> {
        let mut capacity = 1024 + sections.len() * 128;
        loop {
            let mut buf = vec![0u8; capacity];
            match self.write_footer(&mut Writer::new(&mut buf), sections) {
                Ok(written) => {
                    buf.truncate(written);
                    return Ok(buf);
                }
                Err(iris_abi::Error::BufferFull { .. }) => capacity *= 2,
                Err(other) => return Err(other.into()),
            }
        }
    }

    fn write_footer(
        &self,
        w: &mut Writer<'_>,
        sections: &[Section],
    ) -> core::result::Result<usize, iris_abi::Error> {
        w.record(tag::DATASET, Dataset::VERSION, |w| self.dataset.encode(w))?;
        if let Some((encoding, bytes)) = &self.schema {
            let schema = Schema {
                encoding: *encoding,
                bytes,
            };
            w.record(tag::SCHEMA, Schema::VERSION, |w| schema.encode(w))?;
        }
        if let Some(decoder) = &self.decoder {
            let reference = DecoderRef {
                abi_major: decoder.abi_major,
                abi_minor: decoder.abi_minor,
                location: decoder.location,
                digest: decoder.digest,
                required: decoder.required,
                name: &decoder.name,
            };
            w.record(tag::DECODER, DecoderRef::VERSION, |w| reference.encode(w))?;
        }
        for section in sections {
            w.record(tag::SECTION, Section::VERSION, |w| section.encode(w))?;
        }
        Ok(w.position())
    }
}

/// A writer that remembers how far into the file it is.
///
/// Every offset in a container is an offset from the start of the file, and a writer does not know
/// where it is. Counting here rather than asking the writer is what lets this work over a pipe, a
/// file and a `Vec` without any of them having to answer the same question three different ways.
struct Counted<W> {
    inner: W,
    at: u64,
}

impl<W: std::io::Write> Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.write_all(bytes).map_err(|err| Error::io(&err))?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// Pads out to the next eight byte boundary, so that a section starts somewhere a decoder can
    /// point a wider load at.
    ///
    /// A byte at a time, and at most seven of them. Doing it that way keeps the arithmetic in file
    /// offsets, which are sixty four bits wide because a container is allowed to be longer than this
    /// host can address, and seven one byte writes cost nothing next to the section behind them.
    fn pad(&mut self) -> Result<()> {
        while !self.at.is_multiple_of(ALIGN) {
            self.write(&[0])?;
        }
        Ok(())
    }
}

/// What every section starts on, as a file offset rather than as a length.
const ALIGN: u64 = wire::ALIGN as u64;
