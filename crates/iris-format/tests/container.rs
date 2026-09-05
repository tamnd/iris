//! What the container promises, written as tests so that breaking one is a red build.
//!
//! The theme of this file is that a container is something somebody else wrote. Half of these tests
//! are about what happens when that somebody was careless and half are about what happens when they
//! were not careless at all.
//!
//! The casts between `u64` and `usize` in the helpers below are the ones the crate itself refuses
//! to make. Here they are fine: the inputs are containers this file built a moment ago, on a
//! machine running this test suite, and a test that has to handle a four gigabyte offset is a test
//! about the wrong thing.
#![allow(clippy::cast_possible_truncation)]

use iris_format::{
    Builder, Container, DecoderLocation, Digest, Error, MAGIC, SchemaEncoding, SectionKind,
};

const TRAILER_SIZE: usize = 56;
const HEADER_SIZE: usize = 16;

/// A container with a bit of everything in it.
fn sample() -> Vec<u8> {
    let mut builder = Builder::new("readings", 1_024);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        b"pretend this is an arrow schema".to_vec(),
    );
    builder.section(SectionKind::Data, vec![7u8; 300]);
    builder.section(SectionKind::Sidecar, b"an index".to_vec());
    builder.embed_decoder(
        "readings-v1",
        (0, 1),
        iris_abi::CapabilitySet::new().with(iris_abi::Capability::REQUIRE_RANGE),
        b"\0asm\x01\0\0\0".to_vec(),
    );
    builder.build().expect("this builds")
}

fn footer_range(bytes: &[u8]) -> (usize, usize) {
    let t = bytes.len() - TRAILER_SIZE;
    let offset = u64::from_le_bytes(bytes[t..t + 8].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(bytes[t + 8..t + 12].try_into().unwrap()) as usize;
    (offset, offset + len)
}

/// Recomputes the root digest after a test has edited the metadata.
///
/// Without this, every test below would fail at the digest check and would prove only that the
/// digest check works, which is one test and not fifteen.
fn reseal(bytes: &mut [u8]) {
    let (start, end) = footer_range(bytes);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes[..HEADER_SIZE]);
    hasher.update(&bytes[start..end]);
    let root = *hasher.finalize().as_bytes();
    let t = bytes.len() - TRAILER_SIZE;
    bytes[t + 16..t + 48].copy_from_slice(&root);
}

/// Walks the footer and returns where the payload of the first record with this tag starts.
fn payload_of(bytes: &[u8], tag: u16) -> usize {
    let (start, end) = footer_range(bytes);
    let mut at = start;
    while at + 8 <= end {
        let found = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if found == tag {
            return at + 8;
        }
        at += 8 + len.next_multiple_of(8);
    }
    panic!("no record with tag {tag:#06x} in the footer");
}

const TAG_DATASET: u16 = 0x0100;
const TAG_SECTION: u16 = 0x0103;

#[test]
fn a_container_round_trips() {
    let bytes = sample();
    let container = Container::parse(&bytes).expect("this parses");
    container.verify().expect("nothing has been edited");

    assert_eq!(container.dataset().rows, 1_024);
    assert_eq!(container.dataset().name, "readings");
    assert_eq!(
        container.schema().unwrap().encoding,
        SchemaEncoding::ArrowIpc
    );
    assert_eq!(
        container.schema().unwrap().bytes,
        b"pretend this is an arrow schema"
    );
    assert_eq!(container.sections().len(), 3);
    assert_eq!(
        container.section_bytes(container.section(0).unwrap()),
        [7u8; 300]
    );
    assert_eq!(
        container.section_bytes(container.section(1).unwrap()),
        b"an index"
    );
}

#[test]
fn every_section_starts_on_an_eight_byte_boundary() {
    let bytes = sample();
    let container = Container::parse(&bytes).unwrap();
    for section in container.sections() {
        assert_eq!(
            section.offset % 8,
            0,
            "section {} starts at {}",
            section.id,
            section.offset
        );
    }
}

#[test]
fn an_embedded_decoder_comes_back_by_digest() {
    let bytes = sample();
    let container = Container::parse(&bytes).unwrap();

    let decoder = container.decoder().expect("there is a decoder");
    assert_eq!(decoder.name, "readings-v1");
    assert_eq!((decoder.abi_major, decoder.abi_minor), (0, 1));
    assert!(
        decoder
            .required
            .contains(iris_abi::Capability::REQUIRE_RANGE)
    );
    assert!(matches!(decoder.location, DecoderLocation::Embedded { .. }));

    let module = container.decoder_bytes().expect("it is embedded");
    assert_eq!(module, b"\0asm\x01\0\0\0");
    // The digest in the footer is the identity a host substitutes a native decoder on, so it has to
    // be the digest of the bytes that are actually there and not of anything else.
    assert_eq!(Digest::of(module), decoder.digest);
}

#[test]
fn an_external_decoder_carries_a_digest_and_no_bytes() {
    let mut builder = Builder::new("readings", 1);
    builder.external_decoder(
        "shared-v3",
        (0, 1),
        iris_abi::CapabilitySet::new(),
        Digest::of(b"the module lives somewhere else"),
    );
    let bytes = builder.build().unwrap();

    let container = Container::parse(&bytes).unwrap();
    let decoder = container.decoder().unwrap();
    assert_eq!(decoder.location, DecoderLocation::External);
    assert_eq!(
        decoder.digest,
        Digest::of(b"the module lives somewhere else")
    );
    assert!(container.decoder_bytes().is_none());
}

/// Truncate a good container at every possible length and check that not one of them panics.
#[test]
fn a_truncated_container_never_panics() {
    let bytes = sample();
    for cut in 0..bytes.len() {
        let _ = Container::parse(&bytes[..cut]);
    }
}

/// Flip one bit in every byte of the metadata and check that every one of them is caught.
///
/// The payload area is skipped because a change there is caught by `verify` rather than by `parse`,
/// which is a different test.
#[test]
fn a_bit_flipped_in_the_metadata_is_caught() {
    let bytes = sample();
    let (footer_start, footer_end) = footer_range(&bytes);
    let metadata: Vec<usize> = (0..HEADER_SIZE)
        .chain(footer_start..footer_end)
        .chain(bytes.len() - TRAILER_SIZE..bytes.len())
        .collect();

    for at in metadata {
        let mut broken = bytes.clone();
        broken[at] ^= 0x01;
        assert!(
            Container::parse(&broken).is_err(),
            "a flipped bit at offset {at} parsed anyway"
        );
    }
}

#[test]
fn a_bit_flipped_in_a_section_is_caught_by_verify_and_not_by_parse() {
    let mut bytes = sample();
    let container = Container::parse(&bytes).unwrap();
    let at = container.section(0).unwrap().offset as usize;
    drop(container);

    bytes[at] ^= 0x01;

    // Parsing still works, because the metadata is untouched and parsing does not read the payload.
    // That is the point of the split: opening a hundred gigabyte dataset should not hash it.
    let container = Container::parse(&bytes).expect("the metadata is still intact");
    let error = container.verify().expect_err("the section has changed");
    assert!(
        matches!(&error, Error::DigestMismatch { what, .. } if what == "section 0"),
        "expected a digest mismatch naming section 0, got {error}"
    );
}

#[test]
fn a_section_that_points_outside_the_file_is_refused() {
    let mut bytes = sample();
    let payload = payload_of(&bytes, TAG_SECTION);
    // Field layout is id, kind, then offset at eight.
    bytes[payload + 8..payload + 16].copy_from_slice(&1_000_000u64.to_le_bytes());
    reseal(&mut bytes);

    let error = Container::parse(&bytes).expect_err("that section is not in the file");
    assert!(
        matches!(error, Error::SectionOutOfBounds { .. }),
        "got {error}"
    );
}

#[test]
fn a_section_whose_offset_and_length_overflow_is_refused() {
    let mut bytes = sample();
    let payload = payload_of(&bytes, TAG_SECTION);
    bytes[payload + 8..payload + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    bytes[payload + 16..payload + 24].copy_from_slice(&64u64.to_le_bytes());
    reseal(&mut bytes);

    // The interesting failure mode is not the error, it is that the addition of those two numbers
    // wraps to 63 and the range looks small and reasonable.
    let error = Container::parse(&bytes).expect_err("that section wraps");
    assert!(
        matches!(error, Error::SectionOutOfBounds { end: u64::MAX, .. }),
        "got {error}"
    );
}

#[test]
fn a_section_may_not_overlap_the_footer() {
    let mut bytes = sample();
    let (footer_start, _) = footer_range(&bytes);
    let payload = payload_of(&bytes, TAG_SECTION);
    bytes[payload + 8..payload + 16].copy_from_slice(&(footer_start as u64 - 8).to_le_bytes());
    bytes[payload + 16..payload + 24].copy_from_slice(&64u64.to_le_bytes());
    reseal(&mut bytes);

    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::SectionOutOfBounds { .. })
    ));
}

#[test]
fn a_repeated_section_id_is_refused() {
    let mut bytes = sample();
    let (footer_start, footer_end) = footer_range(&bytes);
    // Set the id of every section record to zero, so at least two of them collide.
    let mut at = footer_start;
    while at + 8 <= footer_end {
        let tag = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if tag == TAG_SECTION {
            bytes[at + 8..at + 12].copy_from_slice(&0u32.to_le_bytes());
        }
        at += 8 + len.next_multiple_of(8);
    }
    reseal(&mut bytes);

    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::DuplicateSection { id: 0 })
    ));
}

/// A footer length that no file could hold must be refused before anything is allocated for it.
///
/// There is nothing clever to assert here, because the failure this guards against is the process
/// being killed by the out of memory killer rather than an assertion failing. What the test does is
/// make that failure reproducible if it ever comes back.
#[test]
fn a_footer_length_larger_than_the_file_allocates_nothing() {
    let mut bytes = sample();
    let t = bytes.len() - TRAILER_SIZE;
    bytes[t + 8..t + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    // No reseal here. The bounds check happens before the digest check, which is the order that
    // matters: a digest is computed over a range, and computing one over a range that was never
    // checked is how a bounds check gets skipped by accident.

    let error = Container::parse(&bytes).expect_err("no file is four gigabytes of footer");
    assert!(matches!(error, Error::Truncated { .. }), "got {error}");
}

#[test]
fn a_footer_that_starts_inside_the_header_is_refused() {
    let mut bytes = sample();
    let t = bytes.len() - TRAILER_SIZE;
    bytes[t..t + 8].copy_from_slice(&0u64.to_le_bytes());
    reseal(&mut bytes);

    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::Truncated { .. })
    ));
}

#[test]
fn an_unknown_footer_record_is_stepped_over() {
    // Build a container, then splice an unknown record into the middle of its footer, the way a
    // newer writer would. Everything that was readable before must still be readable.
    let bytes = sample();
    let (footer_start, footer_end) = footer_range(&bytes);

    let mut extra = Vec::new();
    extra.extend_from_slice(&0x0F00u16.to_le_bytes());
    extra.extend_from_slice(&3u16.to_le_bytes());
    extra.extend_from_slice(&16u32.to_le_bytes());
    extra.extend_from_slice(b"from the future!");

    let mut grown = Vec::new();
    grown.extend_from_slice(&bytes[..footer_end]);
    grown.extend_from_slice(&extra);
    let new_footer_len = (footer_end - footer_start + extra.len()) as u32;
    grown.extend_from_slice(&bytes[footer_end..]);
    let t = grown.len() - TRAILER_SIZE;
    grown[t + 8..t + 12].copy_from_slice(&new_footer_len.to_le_bytes());
    reseal(&mut grown);

    let container = Container::parse(&grown).expect("an unknown record is not a reason to stop");
    assert_eq!(container.dataset().name, "readings");
    assert_eq!(container.sections().len(), 3);
    container.verify().expect("the sections did not move");
}

#[test]
fn a_footer_record_from_a_newer_version_of_a_record_we_know_is_refused() {
    let mut bytes = sample();
    let payload = payload_of(&bytes, TAG_DATASET);
    bytes[payload - 6..payload - 4].copy_from_slice(&9u16.to_le_bytes());
    reseal(&mut bytes);

    // Unknown tag means skip. Known tag at an unknown version means stop, because the fields are
    // in the same place but they no longer mean the same thing, and reading them anyway is how a
    // reader produces a confident wrong answer.
    let error = Container::parse(&bytes).expect_err("version nine is not version one");
    assert!(
        matches!(error, Error::UnsupportedRecord { version: 9, .. }),
        "got {error}"
    );
}

#[test]
fn a_container_with_no_dataset_record_is_refused() {
    let mut bytes = sample();
    let payload = payload_of(&bytes, TAG_DATASET);
    bytes[payload - 8..payload - 6].copy_from_slice(&0xF00Du16.to_le_bytes());
    reseal(&mut bytes);

    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::MissingRecord(_))
    ));
}

#[test]
fn something_that_is_not_a_container_says_so() {
    let error = Container::parse(b"this is a parquet file, honestly, all the way to the end of it")
        .expect_err("it is not");
    assert!(matches!(error, Error::NotAContainer { .. }), "got {error}");

    // Short inputs get the same answer rather than a truncation error, because telling somebody
    // their PNG is a truncated iris container helps nobody.
    assert!(matches!(
        Container::parse(b"\x89PNG"),
        Err(Error::NotAContainer { .. })
    ));

    // Something that starts right and stops immediately is truncated, which is a different problem
    // and gets a different sentence.
    assert!(matches!(
        Container::parse(&MAGIC),
        Err(Error::Truncated { .. })
    ));
}

#[test]
fn a_container_from_a_newer_major_version_is_refused() {
    let mut bytes = sample();
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    reseal(&mut bytes);

    let error = Container::parse(&bytes).expect_err("format 1 is not format 0");
    assert!(
        matches!(error, Error::UnsupportedFormat { major: 1, .. }),
        "got {error}"
    );
}

#[test]
fn a_reserved_field_that_is_not_zero_is_refused() {
    let mut header = sample();
    header[12..16].copy_from_slice(&1u32.to_le_bytes());
    reseal(&mut header);
    assert!(matches!(
        Container::parse(&header),
        Err(Error::Reserved { what: "header" })
    ));

    let mut trailer = sample();
    let t = trailer.len() - TRAILER_SIZE;
    trailer[t + 12..t + 16].copy_from_slice(&1u32.to_le_bytes());
    reseal(&mut trailer);
    assert!(matches!(
        Container::parse(&trailer),
        Err(Error::Reserved { what: "trailer" })
    ));
}

/// A lot of arbitrary bytes, none of which are allowed to panic.
///
/// This is not a substitute for the fuzz target, it is the part of it that runs on every commit.
/// The generator is a fixed sequence so a failure here is reproducible without a corpus file.
#[test]
fn arbitrary_bytes_never_panic() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let good = sample();
    for _ in 0..2_000 {
        let len = (next() % 400) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
        let _ = Container::parse(&buf);

        // Also a real container with a handful of bytes stamped on, which reaches much further into
        // the parser than random noise does.
        buf = good.clone();
        for _ in 0..8 {
            let at = (next() as usize) % buf.len();
            buf[at] = (next() & 0xff) as u8;
        }
        let _ = Container::parse(&buf);
        if let Ok(container) = Container::parse(&buf) {
            let _ = container.verify();
            for section in container.sections() {
                let _ = container.section_bytes(section);
            }
        }
    }
}

#[test]
fn a_container_written_out_is_the_container_that_was_built() {
    // The two ways out of the builder have to agree byte for byte, because the gate that reads a
    // four gigabyte file writes it with one of them and checks it against a container built with
    // the other. A difference of a single padding byte would move every offset after it.
    let mut builder = Builder::new("readings", 1_024);
    builder.schema(SchemaEncoding::ArrowIpc, b"a schema".to_vec());
    builder.section(SectionKind::Data, vec![7u8; 300]);
    builder.section(SectionKind::Sidecar, b"an index".to_vec());
    builder.embed_decoder(
        "readings-v1",
        (0, 1),
        iris_abi::CapabilitySet::new(),
        b"\0asm\x01\0\0\0".to_vec(),
    );

    let collected = builder.build().expect("this builds");
    let mut written = Vec::new();
    let len = builder.build_into(&mut written).expect("this writes");

    assert_eq!(written, collected);
    assert_eq!(len, collected.len() as u64);
    Container::parse(&written)
        .expect("this parses")
        .verify()
        .expect("this verifies");
}

#[test]
fn a_writer_that_refuses_gives_back_what_it_said() {
    /// A writer that has already run out of room.
    struct Full;

    impl std::io::Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut builder = Builder::new("readings", 1);
    builder.section(SectionKind::Data, vec![0u8; 8]);
    let err = builder.build_into(Full).expect_err("this cannot write");
    assert!(
        matches!(err, Error::Io { kind, .. } if kind == std::io::ErrorKind::StorageFull),
        "{err}"
    );
}
