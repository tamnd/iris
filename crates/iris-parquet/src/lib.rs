//! An iris container carried inside a Parquet file, where a reader that has never heard of iris
//! does not have to do anything about it.
//!
//! A file written by [`write()`] is two things at once. It is a plain Parquet file, with the rows in
//! ordinary Parquet encodings, which every Parquet reader in the world reads as it always has. It
//! also carries a whole iris container, with the decoder inside it, and two keys in the file
//! metadata that say where. A reader that knows those keys opens the container and gets the data
//! through the decoder the writer chose. A reader that does not know them reads the Parquet and is
//! not slowed down, warned, or broken by the rest.
//!
//! That is the entire idea, and the reason it is worth having is that it asks nobody to adopt
//! anything. A new format normally has to be read before it is worth writing, and has to be written
//! before it is worth reading, and most formats die in that circle. A file that is already a
//! Parquet file is outside it.
//!
//! # Where the container goes
//!
//! ```text
//! +---------------------------------------------+ 0
//! | PAR1                                        |
//! +---------------------------------------------+ 4
//! | row groups, ordinary Parquet                 |
//! +---------------------------------------------+ at
//! | the iris container, whole                    |
//! +---------------------------------------------+ at + len
//! | the Parquet footer, which says where at is   |
//! +---------------------------------------------+
//! | footer length, PAR1                          |
//! +---------------------------------------------+
//! ```
//!
//! Between the last row group and the footer, and nothing in the footer points at it. A Parquet
//! reader finds the footer from the end of the file and finds every column chunk by an offset the
//! footer gives it, so the bytes in that gap are bytes it never asks for. Writing the container
//! there costs no adjustment to anything: every offset the footer holds was fixed before those
//! bytes existed, and the footer length at the end of the file is measured from the end.
//!
//! The obvious alternative is to put the container in the file metadata itself, base64 encoded,
//! since Parquet's key and value metadata is where extensions are supposed to live. That works and
//! it is worse. Every reader parses the whole footer on open, including the metadata it does not
//! understand, so a container in there is a cost paid by exactly the readers that get nothing back
//! for it, and it is paid on open rather than on scan. The gap costs those readers nothing at all,
//! which is the property the whole idea is built on.
//!
//! What does go in the metadata is two short strings. [`CONTAINER_KEY`] is the offset and the
//! length. [`DECODER_KEY`] is a copy of what the container's own footer says about its decoder, so
//! that a host can decide whether it wants anything to do with this file while it still has only
//! the footer in hand, rather than after a second read.
//!
//! # The hint is a hint
//!
//! The decoder digest in the file metadata is not evidence and must not be treated as any. The
//! container commits to its own footer with a digest and this crate's two keys are outside that, so
//! anybody who can write the file can write whatever they like in them. It is there to answer
//! "should I bother reading further", which is a decision that is safe to get wrong. The decision
//! that is not safe to get wrong, which is whether to substitute a trusted native implementation
//! for the module in the file, is made against the container's own decoder record after opening it,
//! by the host, exactly as it is for a container that arrived on its own.
//!
//! # What survives
//!
//! Nothing about this survives a rewrite. A reader that opens one of these files and writes it back
//! out has written a new Parquet file, and a new Parquet file has no gap and no keys in it. That is
//! the honest behaviour and not a limitation to work around: the container was in that file because
//! a writer put it there, and a writer that does not know about iris has not put one anywhere.

use std::io::{Read, Write};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use iris_format::layout::DIGEST_SIZE;
use iris_format::{Container, Digest};
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::{KeyValue, ParquetMetaData, ParquetMetaDataReader};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::ChunkReader;

/// The key holding the offset and the length of the container, as two decimal numbers with a comma
/// between them.
pub const CONTAINER_KEY: &str = "iris.container";

/// The key holding what the container says about its decoder, as the ABI version, the digest of
/// the module and the name, with a comma between each.
///
/// A copy of the container's own decoder record, present so that a host can decide from the
/// Parquet footer alone. Read the note on hints in the crate documentation before believing any of
/// it.
pub const DECODER_KEY: &str = "iris.decoder";

/// How much of a metadata value an error message repeats.
///
/// The values come out of a file somebody else wrote, so a message that quotes one whole is a
/// message whose length that person chooses.
const QUOTED: usize = 64;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Parquet library said no.
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    /// Reading or writing the file itself.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// A key is there and does not have the shape this crate writes.
    #[error("the {key} metadata in this file is not the shape iris writes: {value}")]
    Malformed {
        /// Which key.
        key: &'static str,
        /// What was in it, cut short.
        value: String,
    },

    /// The file points at a range that is not in the file.
    #[error("this file says its container is {len} bytes at {at}, and the file is {size} bytes")]
    OutOfBounds {
        /// Where the container was said to start.
        at: u64,
        /// How long it was said to be.
        len: u64,
        /// How long the file actually is.
        size: u64,
    },
}

/// What this crate returns.
pub type Result<T> = std::result::Result<T, Error>;

/// Where a container sits inside a Parquet file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Embedded {
    /// The offset of the first byte of the container.
    pub at: u64,
    /// How many bytes of it there are.
    pub len: u64,
}

/// What the file metadata claims about the decoder in the container.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecoderHint {
    /// The ABI major version the decoder was built against.
    pub abi_major: u16,
    /// The ABI minor version the decoder was built against.
    pub abi_minor: u16,
    /// The digest of the module.
    pub digest: Digest,
    /// The name the container gives it.
    pub name: String,
}

/// Writes a Parquet file carrying `container`, with default writer properties.
///
/// The rows in `batches` and the rows in the container are meant to be the same rows. Nothing here
/// checks that, because checking it means decoding the container, which is the host's job and a
/// much larger dependency than this crate wants. A caller that writes two different datasets into
/// one file has written a file that answers two different things depending on who reads it.
///
/// # Errors
///
/// Whatever the Parquet writer says, or whatever the sink says.
pub fn write<W: Write + Send>(
    sink: W,
    schema: &SchemaRef,
    batches: &[RecordBatch],
    container: &[u8],
) -> Result<W> {
    write_with(sink, schema, batches, container, WriterProperties::new())
}

/// Writes a Parquet file carrying `container`, with the writer properties given.
///
/// # Errors
///
/// Whatever the Parquet writer says, or whatever the sink says.
pub fn write_with<W: Write + Send>(
    sink: W,
    schema: &SchemaRef,
    batches: &[RecordBatch],
    container: &[u8],
    properties: WriterProperties,
) -> Result<W> {
    let mut writer = ArrowWriter::try_new(sink, Arc::clone(schema), Some(properties))?;
    for batch in batches {
        writer.write(batch)?;
    }

    // The row group in progress has to be closed before the container goes down, or the container
    // lands in the middle of one and every offset after it is a page somewhere else.
    writer.flush()?;

    let at = writer.bytes_written() as u64;
    writer.write_all(container)?;
    writer.append_key_value_metadata(KeyValue::new(
        CONTAINER_KEY.to_owned(),
        format!("{at},{}", container.len()),
    ));

    // Parsed without checking the root digest, because the digest is checked by whoever opens this
    // for real and a writer that refused to embed an unparseable container would be the second
    // thing checking it. What this needs is the decoder record, and a container with no readable
    // footer simply has no hint to write.
    if let Some(decoder) = Container::parse_without_root_digest(container)
        .ok()
        .and_then(|parsed| parsed.decoder().cloned())
    {
        writer.append_key_value_metadata(KeyValue::new(
            DECODER_KEY.to_owned(),
            format!(
                "{}.{},{},{}",
                decoder.abi_major, decoder.abi_minor, decoder.digest, decoder.name
            ),
        ));
    }

    // Writes the footer and hands the sink back. `close` writes the same footer and returns the
    // metadata instead, which is the thing a caller here already has.
    Ok(writer.into_inner()?)
}

/// Where the container is, according to this file's metadata.
///
/// `Ok(None)` means the file does not claim to carry one, which is the answer for every Parquet
/// file that was written by anything else.
///
/// # Errors
///
/// [`Error::Malformed`] if the key is present and is not two numbers.
pub fn embedded(metadata: &ParquetMetaData) -> Result<Option<Embedded>> {
    let Some(value) = value(metadata, CONTAINER_KEY) else {
        return Ok(None);
    };
    let malformed = || Error::Malformed {
        key: CONTAINER_KEY,
        value: value.chars().take(QUOTED).collect(),
    };

    let (at, len) = value.split_once(',').ok_or_else(malformed)?;
    Ok(Some(Embedded {
        at: at.parse().map_err(|_| malformed())?,
        len: len.parse().map_err(|_| malformed())?,
    }))
}

/// What this file claims about the decoder in the container it carries.
///
/// # Errors
///
/// [`Error::Malformed`] if the key is present and is not the shape this crate writes.
pub fn decoder_hint(metadata: &ParquetMetaData) -> Result<Option<DecoderHint>> {
    let Some(value) = value(metadata, DECODER_KEY) else {
        return Ok(None);
    };
    let malformed = || Error::Malformed {
        key: DECODER_KEY,
        value: value.chars().take(QUOTED).collect(),
    };

    // The name is last and is taken whole, because a name is whatever the container called it and
    // is allowed to have a comma in it.
    let (abi, rest) = value.split_once(',').ok_or_else(malformed)?;
    let (digest, name) = rest.split_once(',').ok_or_else(malformed)?;
    let (major, minor) = abi.split_once('.').ok_or_else(malformed)?;

    let mut bytes = [0u8; DIGEST_SIZE];
    if digest.len() != bytes.len() * 2 {
        return Err(malformed());
    }
    let (pairs, _) = digest.as_bytes().as_chunks::<2>();
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        let pair = std::str::from_utf8(pair).map_err(|_| malformed())?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| malformed())?;
    }

    Ok(Some(DecoderHint {
        abi_major: major.parse().map_err(|_| malformed())?,
        abi_minor: minor.parse().map_err(|_| malformed())?,
        digest: Digest(bytes),
        name: name.to_owned(),
    }))
}

/// Reads the container out of a Parquet file, if it carries one.
///
/// This reads the footer and then the container, and no data pages at all, so the cost is the size
/// of the container rather than the size of the file.
///
/// # Errors
///
/// [`Error::OutOfBounds`] if the file points outside itself, and whatever the reader says.
pub fn container<R: ChunkReader>(reader: &R) -> Result<Option<Vec<u8>>> {
    let metadata = ParquetMetaDataReader::new().parse_and_finish(reader)?;
    let Some(found) = embedded(&metadata)? else {
        return Ok(None);
    };

    // Checked here rather than trusted to the read, because a length out of a file is the number
    // this would otherwise allocate on the strength of, and the file is right there to check it
    // against.
    let size = reader.len();
    if found.len > size || found.at > size - found.len {
        return Err(Error::OutOfBounds {
            at: found.at,
            len: found.len,
            size,
        });
    }

    let len = usize::try_from(found.len).map_err(|_| Error::OutOfBounds {
        at: found.at,
        len: found.len,
        size,
    })?;
    let mut bytes = Vec::with_capacity(len);
    reader
        .get_read(found.at)?
        .take(found.len)
        .read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// One metadata value by key, or nothing.
fn value<'a>(metadata: &'a ParquetMetaData, key: &str) -> Option<&'a str> {
    metadata
        .file_metadata()
        .key_value_metadata()?
        .iter()
        .find(|pair| pair.key == key)?
        .value
        .as_deref()
}
