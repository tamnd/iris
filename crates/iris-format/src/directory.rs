//! Reading a container's metadata without holding the container.
//!
//! A container is a header, the sections, a footer and a trailer, and everything a host needs in
//! order to decide what to do is in three of those four. The sections are the payload and they are
//! the only part that is large. Splitting the metadata out from the bytes it describes is what lets
//! a host open a forty gigabyte file by reading about a kilobyte of it.
//!
//! There are two pieces because they are found in two steps, and the order is forced by the layout.
//! [`Placement`] comes from the last [`Placement::TRAILER_LEN`] bytes of the file and says where
//! the footer is. [`Directory`] comes from the first [`HEADER_SIZE`](crate::layout::HEADER_SIZE)
//! bytes and the footer those bytes point at, and says what the container holds. A host that can
//! read a range twice can open a container of any size.
//!
//! # One parser
//!
//! [`crate::Container`] is this plus the payload. It is built on these types rather than beside
//! them, so there is exactly one piece of code in the workspace that reads untrusted container
//! metadata and exactly one fuzz target that has to cover it. A second parser for the windowed case
//! would have been the easier change and it would have meant that half the parsing in the project
//! was fuzzed.
//!
//! The rules the container parser follows apply here unchanged: nothing is indexed without a bounds
//! check, no arithmetic on a length out of the file is unchecked, and nothing is allocated in
//! proportion to a number the file claims rather than to bytes that are there.

use iris_abi::{Reader, Tag};

use crate::container::FileHeader;
use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::layout::{
    DecoderLocation, FORMAT_MAJOR, HEADER_SIZE, MAGIC, MIN_SIZE, TRAILER_SIZE, tag,
};
use crate::meta::{Dataset, DecoderRef, Schema, Section};

/// Where the footer is, read from the trailer.
///
/// This is the first thing a host learns about a container and the only thing it can learn without
/// being told where to look, because the trailer is at a known distance from the end. Everything
/// else follows from it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    file_len: u64,
    footer_at: u64,
    footer_len: u32,
    root: Digest,
}

impl Placement {
    /// How many bytes at the end of the file the trailer occupies.
    pub const TRAILER_LEN: usize = TRAILER_SIZE;

    /// Where the trailer starts in a file of this length.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] if the file is too short to be a container at all, which is checked
    /// here so that a host cannot be told to read from a negative offset.
    pub fn trailer_at(file_len: u64) -> Result<u64> {
        if file_len < MIN_SIZE as u64 {
            return Err(Error::Truncated {
                what: "the container",
                needed: MIN_SIZE as u64,
                available: file_len,
            });
        }
        Ok(file_len - TRAILER_SIZE as u64)
    }

    /// Reads the trailer of a file of `file_len` bytes.
    ///
    /// `trailer` is the [`Placement::TRAILER_LEN`] bytes at [`Placement::trailer_at`].
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] if the file is too short or does not end with the magic,
    /// [`Error::Reserved`] if a field that has to be zero is not, and [`Error::Truncated`] again if
    /// the footer the trailer names does not fit between the header and the trailer.
    pub fn read(trailer: &[u8], file_len: u64) -> Result<Self> {
        let trailer_at = Self::trailer_at(file_len)?;
        // A fixed size array rather than a slice, so every field below is read at an index the
        // compiler has already checked and there is no way for this to panic on a short read.
        let t: &[u8; TRAILER_SIZE] = trailer.first_chunk().ok_or(Error::Truncated {
            what: "the trailer",
            needed: TRAILER_SIZE as u64,
            available: trailer.len() as u64,
        })?;

        if t[48..56] != MAGIC {
            return Err(Error::Truncated {
                what: "the container, which does not end with the magic",
                needed: file_len,
                available: file_len,
            });
        }

        let footer_at = u64::from_le_bytes([t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7]]);
        let footer_len = u32::from_le_bytes([t[8], t[9], t[10], t[11]]);
        let reserved = u32::from_le_bytes([t[12], t[13], t[14], t[15]]);
        if reserved != 0 {
            return Err(Error::Reserved { what: "trailer" });
        }
        let root: [u8; 32] = core::array::from_fn(|i| t[16 + i]);

        // The footer has to sit between the header and the trailer. Both ends are checked, because
        // a footer that starts inside the header would let a writer describe its own magic as
        // records, and a footer that runs into the trailer would let it describe its own digest.
        let footer_end = footer_at
            .checked_add(u64::from(footer_len))
            .ok_or(Error::Truncated {
                what: "the footer",
                needed: u64::MAX,
                available: file_len,
            })?;
        if footer_at < HEADER_SIZE as u64 || footer_end > trailer_at {
            return Err(Error::Truncated {
                what: "the footer",
                needed: footer_end,
                available: trailer_at,
            });
        }

        Ok(Self {
            file_len,
            footer_at,
            footer_len,
            root: Digest(root),
        })
    }

    /// How long the file is.
    #[must_use]
    pub const fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Where the footer starts.
    #[must_use]
    pub const fn footer_at(&self) -> u64 {
        self.footer_at
    }

    /// How long the footer is.
    ///
    /// A `usize` because it is a `u32` in the file and every target this builds for has a `usize`
    /// at least that wide, so a host can read it in one call without a conversion that could fail.
    #[must_use]
    pub const fn footer_len(&self) -> usize {
        self.footer_len as usize
    }

    /// The digest that covers the header and the footer.
    #[must_use]
    pub const fn root_digest(&self) -> Digest {
        self.root
    }
}

/// What a container holds, without the bytes it holds.
///
/// Parsed from the header and the footer, so it borrows the footer and nothing else. A host that
/// read those two ranges out of a file it is not holding has everything here that a host holding
/// the whole file has, minus the ability to hand over a section's bytes.
#[derive(Clone, Debug)]
pub struct Directory<'a> {
    header: FileHeader,
    raw_header: [u8; HEADER_SIZE],
    footer: &'a [u8],
    placement: Placement,
    dataset: Dataset,
    schema: Option<Schema<'a>>,
    decoder: Option<DecoderRef<'a>>,
    sections: Vec<Section>,
}

impl<'a> Directory<'a> {
    /// Parses the metadata and checks that it is the metadata that was written.
    ///
    /// `header` is the first [`HEADER_SIZE`](crate::layout::HEADER_SIZE) bytes of the file and
    /// `footer` is the [`Placement::footer_len`] bytes at [`Placement::footer_at`]. The root digest
    /// covers exactly those two ranges, so a directory that parses is one nobody has edited since
    /// it was written.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] describing the first thing that was wrong, including
    /// [`Error::DigestMismatch`] if the bytes do not hash to the root digest in the trailer.
    pub fn parse(header: &[u8], footer: &'a [u8], placement: Placement) -> Result<Self> {
        let directory = Self::parse_without_root_digest(header, footer, placement)?;
        let actual = directory.compute_root();
        if actual != placement.root {
            return Err(Error::DigestMismatch {
                what: "footer".to_owned(),
                expected: placement.root.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(directory)
    }

    /// Parses the metadata without checking the root digest.
    ///
    /// This exists for the fuzzer, for the same reason the container has one: checking the digest
    /// first would mean essentially every generated input is rejected before the parser behind it
    /// runs, and the parser is the part that needs the fuzzing.
    ///
    /// # Errors
    ///
    /// The same as [`Directory::parse`], minus the digest mismatch.
    pub fn parse_without_root_digest(
        header: &[u8],
        footer: &'a [u8],
        placement: Placement,
    ) -> Result<Self> {
        let raw_header: [u8; HEADER_SIZE] = *header.first_chunk().ok_or(Error::Truncated {
            what: "the header",
            needed: HEADER_SIZE as u64,
            available: header.len() as u64,
        })?;

        if raw_header[..MAGIC.len()] != MAGIC {
            return Err(Error::NotAContainer {
                found: raw_header[..MAGIC.len()].to_vec(),
                expected: MAGIC.to_vec(),
            });
        }

        // The trailer said how long the footer is, so a footer of a different length means the
        // caller read the wrong range and the records in it would be parsed against the wrong
        // boundary. Better to say so than to parse whatever arrived.
        if footer.len() != placement.footer_len() {
            return Err(Error::Truncated {
                what: "the footer",
                needed: placement.footer_len() as u64,
                available: footer.len() as u64,
            });
        }

        let mut directory = Self {
            header: parse_header(&raw_header)?,
            raw_header,
            footer,
            placement,
            dataset: Dataset {
                rows: 0,
                name: String::new(),
            },
            schema: None,
            decoder: None,
            sections: Vec::new(),
        };
        directory.parse_footer(footer)?;
        directory.check_sections()?;
        Ok(directory)
    }

    fn parse_footer(&mut self, footer: &'a [u8]) -> Result<()> {
        let mut seen_dataset = false;
        let mut p = Reader::new(footer);
        while !p.is_empty() {
            let (header, mut body) = p.record()?;
            match header.tag {
                tag::DATASET => {
                    expect_version(header.tag, header.version, Dataset::VERSION)?;
                    if seen_dataset {
                        return Err(Error::RepeatedRecord(tag::DATASET));
                    }
                    self.dataset = Dataset::decode(&mut body)?;
                    seen_dataset = true;
                }
                tag::SCHEMA => {
                    expect_version(header.tag, header.version, Schema::VERSION)?;
                    if self.schema.is_some() {
                        return Err(Error::RepeatedRecord(tag::SCHEMA));
                    }
                    self.schema = Some(Schema::decode(&mut body)?);
                }
                tag::DECODER => {
                    expect_version(header.tag, header.version, DecoderRef::VERSION)?;
                    if self.decoder.is_some() {
                        return Err(Error::RepeatedRecord(tag::DECODER));
                    }
                    self.decoder = Some(DecoderRef::decode(&mut body)?);
                }
                tag::SECTION => {
                    expect_version(header.tag, header.version, Section::VERSION)?;
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

    fn check_sections(&self) -> Result<()> {
        let file_len = self.placement.file_len();
        for (i, section) in self.sections.iter().enumerate() {
            let end = section.end().ok_or(Error::SectionOutOfBounds {
                id: section.id,
                offset: section.offset,
                end: u64::MAX,
                file_len,
            })?;
            if section.offset < HEADER_SIZE as u64 || end > self.placement.footer_at() {
                return Err(Error::SectionOutOfBounds {
                    id: section.id,
                    offset: section.offset,
                    end,
                    file_len,
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
        hasher.update(&self.raw_header);
        hasher.update(self.footer);
        Digest(*hasher.finalize().as_bytes())
    }

    /// The format version this container was written at.
    #[must_use]
    pub const fn header(&self) -> FileHeader {
        self.header
    }

    /// Where the footer is and how long the file is.
    #[must_use]
    pub const fn placement(&self) -> Placement {
        self.placement
    }

    /// The digest that covers the header and the footer.
    #[must_use]
    pub const fn root_digest(&self) -> Digest {
        self.placement.root_digest()
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

    /// The section the decoder module is in, if the decoder is embedded and that section exists.
    ///
    /// A host holding the whole file follows this with [`crate::Container::section_bytes`]. A host
    /// that is not holding the file reads the range itself, which is the only difference between
    /// the two paths and the reason this returns a section rather than bytes.
    #[must_use]
    pub fn decoder_section(&self) -> Option<&Section> {
        let decoder = self.decoder.as_ref()?;
        let DecoderLocation::Embedded { section } = decoder.location else {
            return None;
        };
        self.section(section)
    }
}

fn parse_header(header: &[u8; HEADER_SIZE]) -> Result<FileHeader> {
    let major = u16::from_le_bytes([header[8], header[9]]);
    let minor = u16::from_le_bytes([header[10], header[11]]);
    if major != FORMAT_MAJOR {
        return Err(Error::UnsupportedFormat {
            major,
            minor,
            supported_major: FORMAT_MAJOR,
        });
    }
    let flags = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    if flags != 0 {
        return Err(Error::Reserved { what: "header" });
    }
    Ok(FileHeader { major, minor })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_shorter_than_the_smallest_container_has_no_trailer_to_read() {
        assert!(matches!(
            Placement::trailer_at(MIN_SIZE as u64 - 1),
            Err(Error::Truncated { .. })
        ));
        assert_eq!(
            Placement::trailer_at(MIN_SIZE as u64).expect("the smallest container has a trailer"),
            (MIN_SIZE - TRAILER_SIZE) as u64
        );
    }

    fn trailer(footer_at: u64, footer_len: u32) -> [u8; TRAILER_SIZE] {
        let mut t = [0u8; TRAILER_SIZE];
        t[0..8].copy_from_slice(&footer_at.to_le_bytes());
        t[8..12].copy_from_slice(&footer_len.to_le_bytes());
        t[48..56].copy_from_slice(&MAGIC);
        t
    }

    #[test]
    fn a_trailer_that_does_not_end_with_the_magic_is_not_a_container() {
        let mut t = trailer(HEADER_SIZE as u64, 8);
        t[55] = 0;
        assert!(matches!(
            Placement::read(&t, 1024),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn a_footer_that_overlaps_the_header_or_the_trailer_is_refused() {
        // Starting inside the header would let a writer describe the magic as records.
        assert!(matches!(
            Placement::read(&trailer(HEADER_SIZE as u64 - 1, 8), 1024),
            Err(Error::Truncated { .. })
        ));
        // Running into the trailer would let it describe its own digest.
        assert!(matches!(
            Placement::read(&trailer(1024 - TRAILER_SIZE as u64, 8), 1024),
            Err(Error::Truncated { .. })
        ));
        // An end that would wrap round saturates into the same refusal.
        assert!(matches!(
            Placement::read(&trailer(u64::MAX, 8), 1024),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn a_reserved_field_that_is_not_zero_is_refused() {
        let mut t = trailer(HEADER_SIZE as u64, 8);
        t[12] = 1;
        assert!(matches!(
            Placement::read(&t, 1024),
            Err(Error::Reserved { what: "trailer" })
        ));
    }

    #[test]
    fn a_footer_of_the_wrong_length_is_refused_rather_than_parsed() {
        let placement = Placement::read(&trailer(HEADER_SIZE as u64, 8), 1024)
            .expect("the placement is well formed");
        let mut header = [0u8; HEADER_SIZE];
        header[..MAGIC.len()].copy_from_slice(&MAGIC);
        header[8..10].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
        assert!(matches!(
            Directory::parse_without_root_digest(&header, &[0u8; 4], placement),
            Err(Error::Truncated {
                what: "the footer",
                ..
            })
        ));
    }
}
