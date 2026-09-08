//! What a file written by this crate looks like to a reader that does not know about iris, and to
//! one that does.
//!
//! The reader used here is arrow-rs, which is the Parquet implementation this crate is built on, so
//! these tests are the cheap half of the claim rather than the whole of it. They say the file is
//! well formed and the container comes back byte for byte. They cannot say that a Parquet reader
//! nobody in this repository wrote is happy with it, because there is only one Parquet reader in
//! this repository. That half is `ci/parquet-embed.sh`, which reads the same file with pyarrow and
//! with DuckDB, and it runs in CI for the same reason this file does.
//!
//! The container in these tests carries bytes where a module would be rather than a decoder that
//! runs. Nothing in this crate opens a container, compiles anything or produces a row, so a real
//! module would be a nested wasm build in aid of a test that would not look at it.

use std::fs::File;
use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet};
use iris_format::{Builder, Digest, SectionKind};
use iris_parquet::{CONTAINER_KEY, DECODER_KEY, Embedded, Error};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::{KeyValue, ParquetMetaDataReader};
use tempfile::NamedTempFile;

/// Rows in the fixture. Small, because none of this is about how much fits.
const ROWS: i64 = 1_000;

/// The name the container gives its decoder, which is the one the hint should come back with.
const DECODER_NAME: &str = "fixedwidth";

/// The schema of everything written here.
fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("c0", DataType::Int64, false),
        Field::new("c1", DataType::Int64, false),
    ]))
}

/// One batch, with the same `column * 1e9 + row` values the rest of the tree uses.
fn batch() -> RecordBatch {
    let c0: Int64Array = (0..ROWS).collect::<Vec<_>>().into();
    let c1: Int64Array = (0..ROWS)
        .map(|row| 1_000_000_000 + row)
        .collect::<Vec<_>>()
        .into();
    RecordBatch::try_new(schema(), vec![Arc::new(c0), Arc::new(c1)])
        .expect("two columns of the declared type")
}

/// One column of a batch, as the integers it holds.
fn column(batch: &RecordBatch, index: usize) -> &Int64Array {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("the schema says this column is an i64")
}

/// A container with a decoder record in it, and bytes where the module would be.
fn container(module: &[u8]) -> Vec<u8> {
    let mut builder = Builder::new("readings", ROWS.unsigned_abs());
    builder.section(SectionKind::Data, vec![7u8; 64]);
    builder.embed_decoder(
        DECODER_NAME,
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        module.to_vec(),
    );
    builder.build().expect("a container this size fits")
}

/// Writes what `write` writes, to a file that goes away afterwards.
fn embedded_file(container: &[u8]) -> NamedTempFile {
    let file = NamedTempFile::new().expect("a scratch file");
    let sink = file.reopen().expect("a second handle to write through");
    iris_parquet::write(sink, &schema(), &[batch()], container).expect("writing the file");
    file
}

#[test]
fn a_parquet_reader_reads_the_rows_and_ignores_the_rest() {
    let file = embedded_file(&container(b"a module goes here"));

    let reader = ParquetRecordBatchReaderBuilder::try_new(file.reopen().expect("reopening"))
        .expect("a file arrow-rs is happy to open")
        .build()
        .expect("a reader");
    let read: Vec<RecordBatch> = reader.map(|batch| batch.expect("a batch")).collect();

    let rows: usize = read.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows, usize::try_from(ROWS).expect("a thousand"));
    assert_eq!(read.first().expect("at least one batch").schema(), schema());

    // Every value, rather than a count of them. A file that came back with the right shape and the
    // wrong bytes is the failure worth catching here, and the container sitting in the middle of it
    // is exactly the thing that would cause it.
    let values: Vec<(i64, i64)> = read
        .iter()
        .flat_map(|batch| {
            let c0 = column(batch, 0);
            let c1 = column(batch, 1);
            (0..batch.num_rows())
                .map(|row| (c0.value(row), c1.value(row)))
                .collect::<Vec<_>>()
        })
        .collect();
    let wanted: Vec<(i64, i64)> = (0..ROWS).map(|row| (row, 1_000_000_000 + row)).collect();
    assert_eq!(values, wanted);
}

#[test]
fn the_container_comes_back_byte_for_byte() {
    let written = container(b"a module goes here");
    let file = embedded_file(&written);

    let read = iris_parquet::container(&file.reopen().expect("reopening"))
        .expect("reading it back")
        .expect("this file carries one");
    assert_eq!(read, written);
}

#[test]
fn the_pointer_is_where_the_container_is() {
    let written = container(b"a module goes here");
    let file = embedded_file(&written);
    let handle = file.reopen().expect("reopening");

    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&handle)
        .expect("a footer");
    let found = iris_parquet::embedded(&metadata)
        .expect("a well formed key")
        .expect("this file carries one");

    assert_eq!(
        found,
        Embedded {
            at: found.at,
            len: written.len() as u64
        }
    );

    // The container starts after the magic and the row groups and ends before the footer, which is
    // what makes it invisible to a reader that goes at the file the way Parquet says to.
    let size = handle.metadata().expect("a size").len();
    assert!(found.at > 4);
    assert!(found.at + found.len < size);
}

#[test]
fn the_decoder_hint_says_what_the_container_says() {
    let module = b"a module goes here";
    let file = embedded_file(&container(module));
    let handle = file.reopen().expect("reopening");

    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&handle)
        .expect("a footer");
    let hint = iris_parquet::decoder_hint(&metadata)
        .expect("a well formed key")
        .expect("this file carries one");

    assert_eq!(hint.abi_major, ABI_MAJOR);
    assert_eq!(hint.abi_minor, ABI_MINOR);
    assert_eq!(hint.name, DECODER_NAME);
    assert_eq!(hint.digest, Digest::of(module));
}

#[test]
fn a_file_nobody_embedded_anything_in_says_so() {
    let file = NamedTempFile::new().expect("a scratch file");
    let sink = file.reopen().expect("a handle to write through");
    let mut writer = ArrowWriter::try_new(sink, schema(), None).expect("a writer");
    writer.write(&batch()).expect("a batch");
    writer.close().expect("a footer");

    let handle = file.reopen().expect("reopening");
    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&handle)
        .expect("a footer");
    assert_eq!(iris_parquet::embedded(&metadata).expect("no key"), None);
    assert_eq!(iris_parquet::decoder_hint(&metadata).expect("no key"), None);
    assert_eq!(iris_parquet::container(&handle).expect("no key"), None);
}

/// Writes a plain file with one key set by hand, which is how a hostile or a broken one arrives.
fn file_with_key(key: &str, value: &str) -> NamedTempFile {
    let file = NamedTempFile::new().expect("a scratch file");
    let sink = file.reopen().expect("a handle to write through");
    let mut writer = ArrowWriter::try_new(sink, schema(), None).expect("a writer");
    writer.write(&batch()).expect("a batch");
    writer.append_key_value_metadata(KeyValue::new(key.to_owned(), value.to_owned()));
    writer.close().expect("a footer");
    file
}

#[test]
fn a_key_that_is_not_two_numbers_is_an_error_and_not_a_guess() {
    for value in ["banana", "12", "12,banana", "", ",", "-1,4"] {
        let file = file_with_key(CONTAINER_KEY, value);
        let handle = file.reopen().expect("reopening");
        let metadata = ParquetMetaDataReader::new()
            .parse_and_finish(&handle)
            .expect("a footer");
        assert!(
            matches!(
                iris_parquet::embedded(&metadata),
                Err(Error::Malformed { .. })
            ),
            "{value} was accepted"
        );
    }
}

#[test]
fn a_decoder_key_that_is_the_wrong_shape_is_an_error_too() {
    for value in [
        "banana",
        "1.0",
        "1.0,notadigest,name",
        "1.0,00,name",
        "x.y,0000000000000000000000000000000000000000000000000000000000000000,name",
    ] {
        let file = file_with_key(DECODER_KEY, value);
        let handle = file.reopen().expect("reopening");
        let metadata = ParquetMetaDataReader::new()
            .parse_and_finish(&handle)
            .expect("a footer");
        assert!(
            matches!(
                iris_parquet::decoder_hint(&metadata),
                Err(Error::Malformed { .. })
            ),
            "{value} was accepted"
        );
    }
}

#[test]
fn a_pointer_outside_the_file_is_refused_rather_than_allocated_for() {
    let file = file_with_key(CONTAINER_KEY, "16,18446744073709551000");
    let handle = file.reopen().expect("reopening");
    assert!(matches!(
        iris_parquet::container(&handle),
        Err(Error::OutOfBounds { .. })
    ));
}

#[test]
fn a_message_about_a_bad_key_does_not_repeat_the_whole_of_it() {
    let file = file_with_key(CONTAINER_KEY, &"x".repeat(4096));
    let handle = file.reopen().expect("reopening");
    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&handle)
        .expect("a footer");
    let message = iris_parquet::embedded(&metadata)
        .expect_err("this is not two numbers")
        .to_string();
    assert!(
        message.len() < 200,
        "the message was {} long",
        message.len()
    );
}

/// A file this crate wrote, read by hand the way the documentation says a reader may.
///
/// The claim in the crate documentation is that the container sits between the last row group and
/// the footer and that nothing in the footer points at it. Reading File without the metadata reader
/// is the only way to check the second half of that.
#[test]
fn nothing_in_the_footer_points_at_the_container() {
    let written = container(b"a module goes here");
    let file = embedded_file(&written);
    let handle = file.reopen().expect("reopening");

    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&handle)
        .expect("a footer");
    let found = iris_parquet::embedded(&metadata)
        .expect("a well formed key")
        .expect("this file carries one");

    for group in metadata.row_groups() {
        for column in group.columns() {
            let (start, len) = column.byte_range();
            assert!(
                start + len <= found.at,
                "a column chunk runs into the container"
            );
        }
    }
}

/// Reading a file with `File` rather than through the tempfile handle, so the path is the one a
/// caller uses.
#[test]
fn a_path_works_the_same_as_a_handle() {
    let written = container(b"a module goes here");
    let file = embedded_file(&written);

    let opened = File::open(file.path()).expect("opening by path");
    let read = iris_parquet::container(&opened)
        .expect("reading it back")
        .expect("this file carries one");
    assert_eq!(read, written);
}
