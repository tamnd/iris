//! Reading a container that is all in memory.
//!
//! This is the untrusted path. Everything behind it is written on the assumption that the bytes
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
//!
//! The parsing itself is in [`Directory`], because a host that cannot hold the file needs
//! exactly the same checks on exactly the same bytes. A [`Container`] is a [`Directory`] plus the
//! payload it describes, and the only thing it adds is the ability to hand a section's bytes over.

use crate::digest::Digest;
use crate::directory::{Directory, Placement};
use crate::error::{Error, Result};
use crate::layout::{HEADER_SIZE, MAGIC, MIN_SIZE};
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
    directory: Directory<'a>,
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
        Self::open(bytes, Directory::parse)
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
        Self::open(bytes, Directory::parse_without_root_digest)
    }

    /// Finds the three ranges the metadata lives in and hands them to one of the two parsers.
    ///
    /// The size and magic checks are here rather than in [`Directory`] because they are the
    /// questions a host asks about a file, and a host reading through a window answers them from
    /// the file length and the header range instead.
    fn open(
        bytes: &'a [u8],
        parse: fn(&[u8], &'a [u8], Placement) -> Result<Directory<'a>>,
    ) -> Result<Self> {
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

        let file_len = bytes.len() as u64;
        let trailer_at =
            usize::try_from(Placement::trailer_at(file_len)?).map_err(|_| Error::TooLarge {
                what: "the trailer offset",
                needed: file_len,
            })?;
        let placement = Placement::read(&bytes[trailer_at..], file_len)?;

        // Both ends of the footer were checked against the file length by the placement, so these
        // conversions cannot fail on a target where the whole file is already addressable.
        let start = usize::try_from(placement.footer_at()).map_err(|_| Error::TooLarge {
            what: "the footer offset",
            needed: placement.footer_at(),
        })?;
        let end = start
            .checked_add(placement.footer_len())
            .ok_or(Error::TooLarge {
                what: "the footer",
                needed: u64::MAX,
            })?;
        let footer = bytes.get(start..end).ok_or(Error::Truncated {
            what: "the footer",
            needed: end as u64,
            available: file_len,
        })?;

        Ok(Self {
            bytes,
            directory: parse(&bytes[..HEADER_SIZE], footer, placement)?,
        })
    }

    /// The metadata, without the payload.
    ///
    /// This is what a host hands on to anything that does not need the bytes, which is most of what
    /// sits above this crate.
    #[must_use]
    pub const fn directory(&self) -> &Directory<'a> {
        &self.directory
    }

    /// The format version this container was written at.
    #[must_use]
    pub const fn header(&self) -> FileHeader {
        self.directory.header()
    }

    /// The digest that covers the header and the footer.
    #[must_use]
    pub const fn root_digest(&self) -> Digest {
        self.directory.root_digest()
    }

    /// What the dataset is.
    #[must_use]
    pub const fn dataset(&self) -> &Dataset {
        self.directory.dataset()
    }

    /// The schema, if there is one.
    #[must_use]
    pub const fn schema(&self) -> Option<&Schema<'a>> {
        self.directory.schema()
    }

    /// The decoder reference, if there is one.
    #[must_use]
    pub const fn decoder(&self) -> Option<&DecoderRef<'a>> {
        self.directory.decoder()
    }

    /// Every section, in the order the footer listed them.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        self.directory.sections()
    }

    /// The section with this id.
    #[must_use]
    pub fn section(&self, id: u32) -> Option<&Section> {
        self.directory.section(id)
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
        self.directory
            .decoder_section()
            .map(|section| self.section_bytes(section))
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
        for section in self.sections() {
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
