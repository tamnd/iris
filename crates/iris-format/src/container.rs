//! Reading a container.
//!
//! This is the untrusted path. Everything in this file is written on the assumption that the bytes
//! were produced by somebody who wants them to be trouble. The rules it follows are short enough to
//! check by reading:
//!
//! - no indexing that is not bounds checked first, and no `unsafe` at all
//! - no arithmetic on a length from the file that is not `checked_` or `saturating_`
//! - nothing is allocated in proportion to a number read out of the file, only in proportion to
//!   bytes that are actually there
//!
//! The third rule is the one that gets skipped in hand written parsers, and it is the one that
//! turns a sixty byte file into an out of memory kill. There is no count field anywhere in the
//! footer for exactly this reason: the number of sections is however many section records the
//! footer actually contains, so a liar has to pay for every one.

use iris_abi::{Reader, Tag};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::layout::{
    DecoderLocation, FORMAT_MAJOR, HEADER_SIZE, MAGIC, MIN_SIZE, TRAILER_SIZE, tag,
};
use crate::meta::{Dataset, DecoderRef, Schema, Section};

/// The fixed fields at the front of a container.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileHeader {
    /// The format major version.
    pub major: u16,
    /// The format minor version.
    pub minor: u16,
}

/// A parsed container, borrowing the bytes it was parsed from.
///
/// Parsing does not copy the payload and does not read it. A container of a hundred gigabytes
/// parses in the time it takes to hash a footer, and the sections are read when somebody asks for
/// them.
#[derive(Clone, Debug)]
pub struct Container<'a> {
    bytes: &'a [u8],
    header: FileHeader,
    footer_range: (usize, usize),
    root: Digest,
    dataset: Dataset,
    schema: Option<Schema<'a>>,
    decoder: Option<DecoderRef<'a>>,
    sections: Vec<Section>,
}

impl<'a> Container<'a> {
    /// Parses a container and checks that the footer is the footer that was written.
    ///
    /// The root digest covers the header and the footer, so a container that parses has metadata
    /// nobody has edited since it was written. Section contents are not read here. Call
    /// [`Container::verify`] for that, which is the expensive one and is a separate decision.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] describing the first thing that was wrong. It never panics, whatever the
    /// input is, and there is a fuzz target that exists to keep that true.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let container = Self::parse_without_root_digest(bytes)?;
        let actual = container.compute_root();
        if actual != container.root {
            return Err(Error::DigestMismatch {
                what: "footer".to_owned(),
                expected: container.root.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(container)
    }

    /// Parses a container without checking the root digest.
    ///
    /// This exists for the fuzzer. Checking the digest first would mean essentially every generated
    /// input is rejected in the trailer, and the parser behind it would never be reached, which is
    /// the part that needs the fuzzing. It is public because the fuzz target lives outside this
    /// crate, and it is named at length so that nobody reaches for it by accident.
    ///
    /// # Errors
    ///
    /// The same as [`Container::parse`], minus the digest mismatch.
    pub fn parse_without_root_digest(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < MIN_SIZE {
            let head = &bytes[..bytes.len().min(MAGIC.len())];
            if !MAGIC.starts_with(head) {
                return Err(Error::NotAContainer {
                    found: head.to_vec(),
                    expected: MAGIC.to_vec(),
                });
            }
            return Err(Error::Truncated {
                what: "the container",
                needed: MIN_SIZE as u64,
                available: bytes.len() as u64,
            });
        }

        if bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::NotAContainer {
                found: bytes[..MAGIC.len()].to_vec(),
                expected: MAGIC.to_vec(),
            });
        }

        let header = Self::parse_header(bytes)?;
        let (footer_offset, footer_len, root) = Self::parse_trailer(bytes)?;

        // The footer has to sit between the header and the trailer. Both ends are checked, because
        // a footer that starts inside the header would let a writer describe its own magic as
        // records, and a footer that runs into the trailer would let it describe its own digest.
        let trailer_at = bytes.len() - TRAILER_SIZE;
        let footer_end = footer_offset
            .checked_add(footer_len)
            .ok_or(Error::Truncated {
                what: "the footer",
                needed: u64::MAX,
                available: bytes.len() as u64,
            })?;
        if footer_offset < HEADER_SIZE as u64 || footer_end > trailer_at as u64 {
            return Err(Error::Truncated {
                what: "the footer",
                needed: footer_end,
                available: trailer_at as u64,
            });
        }
        let start = usize::try_from(footer_offset).map_err(|_| Error::TooLarge {
            what: "the footer offset",
            needed: footer_offset,
        })?;
        let end = usize::try_from(footer_end).map_err(|_| Error::TooLarge {
            what: "the footer",
            needed: footer_end,
        })?;

        let mut container = Self {
            bytes,
            header,
            footer_range: (start, end),
            root,
            dataset: Dataset {
                rows: 0,
                name: String::new(),
            },
            schema: None,
            decoder: None,
            sections: Vec::new(),
        };
        container.parse_footer(&bytes[start..end])?;
        container.check_sections(footer_offset)?;
        Ok(container)
    }

    fn parse_header(bytes: &[u8]) -> Result<FileHeader> {
        let major = u16::from_le_bytes([bytes[8], bytes[9]]);
        let minor = u16::from_le_bytes([bytes[10], bytes[11]]);
        if major != FORMAT_MAJOR {
            return Err(Error::UnsupportedFormat {
                major,
                minor,
                supported_major: FORMAT_MAJOR,
            });
        }
        let flags = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if flags != 0 {
            return Err(Error::Reserved { what: "header" });
        }
        Ok(FileHeader { major, minor })
    }

    fn parse_trailer(bytes: &[u8]) -> Result<(u64, u64, Digest)> {
        let at = bytes.len() - TRAILER_SIZE;
        let t = &bytes[at..];
        if t[48..56] != MAGIC {
            return Err(Error::Truncated {
                what: "the container, which does not end with the magic",
                needed: bytes.len() as u64,
                available: bytes.len() as u64,
            });
        }
        let footer_offset = u64::from_le_bytes(t[0..8].try_into().expect("eight bytes"));
        let footer_len = u64::from(u32::from_le_bytes(t[8..12].try_into().expect("four bytes")));
        let reserved = u32::from_le_bytes(t[12..16].try_into().expect("four bytes"));
        if reserved != 0 {
            return Err(Error::Reserved { what: "trailer" });
        }
        let mut root = [0u8; 32];
        root.copy_from_slice(&t[16..48]);
        Ok((footer_offset, footer_len, Digest(root)))
    }

    fn parse_footer(&mut self, footer: &'a [u8]) -> Result<()> {
        let mut seen_dataset = false;
        let mut p = Reader::new(footer);
        while !p.is_empty() {
            let (header, mut body) = p.record()?;
            match header.tag {
                tag::DATASET => {
                    Self::expect_version(header.tag, header.version, Dataset::VERSION)?;
                    if seen_dataset {
                        return Err(Error::RepeatedRecord(tag::DATASET));
                    }
                    self.dataset = Dataset::decode(&mut body)?;
                    seen_dataset = true;
                }
                tag::SCHEMA => {
                    Self::expect_version(header.tag, header.version, Schema::VERSION)?;
                    if self.schema.is_some() {
                        return Err(Error::RepeatedRecord(tag::SCHEMA));
                    }
                    self.schema = Some(Schema::decode(&mut body)?);
                }
                tag::DECODER => {
                    Self::expect_version(header.tag, header.version, DecoderRef::VERSION)?;
                    if self.decoder.is_some() {
                        return Err(Error::RepeatedRecord(tag::DECODER));
                    }
                    self.decoder = Some(DecoderRef::decode(&mut body)?);
                }
                tag::SECTION => {
                    Self::expect_version(header.tag, header.version, Section::VERSION)?;
                    self.sections.push(Section::decode(&mut body)?);
                }
                // An unknown record is stepped over, which is the whole reason the footer is framed
                // this way. A tool that only wants the section table should not be stopped by a
                // record a newer writer added.
                _ => {}
            }
        }
        if !seen_dataset {
            return Err(Error::MissingRecord(tag::DATASET));
        }
        Ok(())
    }

    fn expect_version(tag: Tag, found: u16, supported: u16) -> Result<()> {
        if found > supported {
            return Err(Error::UnsupportedRecord {
                tag,
                version: found,
            });
        }
        Ok(())
    }

    fn check_sections(&self, footer_offset: u64) -> Result<()> {
        for (i, section) in self.sections.iter().enumerate() {
            let end = section.end().ok_or(Error::SectionOutOfBounds {
                id: section.id,
                offset: section.offset,
                end: u64::MAX,
                file_len: self.bytes.len() as u64,
            })?;
            if section.offset < HEADER_SIZE as u64 || end > footer_offset {
                return Err(Error::SectionOutOfBounds {
                    id: section.id,
                    offset: section.offset,
                    end,
                    file_len: self.bytes.len() as u64,
                });
            }
            if self.sections[..i].iter().any(|s| s.id == section.id) {
                return Err(Error::DuplicateSection { id: section.id });
            }
        }
        Ok(())
    }

    fn compute_root(&self) -> Digest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.bytes[..HEADER_SIZE]);
        hasher.update(&self.bytes[self.footer_range.0..self.footer_range.1]);
        Digest(*hasher.finalize().as_bytes())
    }

    /// The format version this container was written at.
    #[must_use]
    pub const fn header(&self) -> FileHeader {
        self.header
    }

    /// The digest that covers the header and the footer.
    #[must_use]
    pub const fn root_digest(&self) -> Digest {
        self.root
    }

    /// What the dataset is.
    #[must_use]
    pub const fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    /// The schema, if there is one.
    #[must_use]
    pub const fn schema(&self) -> Option<&Schema<'a>> {
        self.schema.as_ref()
    }

    /// The decoder reference, if there is one.
    #[must_use]
    pub const fn decoder(&self) -> Option<&DecoderRef<'a>> {
        self.decoder.as_ref()
    }

    /// Every section, in the order the footer listed them.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The section with this id.
    #[must_use]
    pub fn section(&self, id: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }

    /// The bytes of a section.
    ///
    /// The bounds were checked during parsing, so this cannot be out of range for a section that
    /// came from this container. It takes a `&Section` rather than an id so that the only way to
    /// call it is with one that did.
    #[must_use]
    pub fn section_bytes(&self, section: &Section) -> &'a [u8] {
        let start = usize::try_from(section.offset).unwrap_or(usize::MAX);
        let end = usize::try_from(section.end().unwrap_or(u64::MAX)).unwrap_or(usize::MAX);
        self.bytes.get(start..end).unwrap_or(&[])
    }

    /// The bytes of the embedded decoder module, if the decoder is embedded and the section it
    /// names exists.
    #[must_use]
    pub fn decoder_bytes(&self) -> Option<&'a [u8]> {
        let decoder = self.decoder.as_ref()?;
        let DecoderLocation::Embedded { section } = decoder.location else {
            return None;
        };
        self.section(section).map(|s| self.section_bytes(s))
    }

    /// Hashes every section and checks it against the footer.
    ///
    /// This reads the whole file, so it is a decision rather than something that happens on every
    /// open. The honest place for it is once when a dataset arrives and then never again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DigestMismatch`] naming the first section whose bytes do not hash to what
    /// the footer says they should.
    pub fn verify(&self) -> Result<()> {
        for section in &self.sections {
            let actual = Digest::of(self.section_bytes(section));
            if actual != section.digest {
                return Err(Error::DigestMismatch {
                    what: format!("section {}", section.id),
                    expected: section.digest.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        Ok(())
    }
}
