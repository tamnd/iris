//! The M4 gate: a dataset read through a window a fraction of its size.
//!
//! The four gigabyte ceiling in the prior art is the wasm32 address space, and it is a ceiling on
//! how much of a file a decoder can hold, not on how much of one it can read. This file is about
//! the difference. The decoder is the same `fixedwidth` example the M1 gate runs, compiled the same
//! way, with nothing in it that knows whether the bytes it asks for came from a buffer the host
//! copied in or from a mapping that moves. That is the claim: the ceiling goes away in the host.
//!
//! Two of these tests are quick and one is not. The quick ones run everywhere and say that a
//! container read through a window decodes to what the same container decodes to when it is
//! resident. The slow one writes a container whose data section is larger than the ceiling and
//! reads rows from past it, and it is ignored by default so that no hosted runner ever writes four
//! gigabytes. `.github/workflows/fleet.yml` runs it by name on a machine with the disk for it.
//!
//! The decoders are compiled rather than checked in. See `tests/support/mod.rs`.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet};
use iris_format::{Builder, Directory, Placement, SchemaEncoding, SectionKind};
use iris_runtime::{Runtime, schema_to_ipc};
use iris_source::{FileSource, RangeSource};

mod support;

use support::{decoder_module, workspace_root};

/// The fixed width decoder's header: how many rows, then how many columns.
const HEADER: u64 = 16;

/// The width of every value the fixture holds.
const WIDTH: u64 = 8;

/// Rows in the small fixture, which both paths read.
const ROWS: u64 = 500;

/// Columns in the small fixture. More than one, so a column offset that is wrong by a header is
/// wrong by a whole column here rather than by nothing.
const COLUMNS: u64 = 3;

/// Small enough that a scan comes back in several batches, which is when a window has to move.
const BATCH_ROWS: u64 = 128;

/// Rows in the large fixture, chosen so that its data section is a little over four gigabytes.
///
/// Four gigabytes is not a round number of rows and it does not have to be. What matters is that
/// rows live past the ceiling: at eight bytes a row this is 4,296,000,016 bytes, which is about a
/// megabyte more than a wasm32 guest could address even if it gave its whole memory to the file.
const HUGE_ROWS: u64 = 537_000_000;

/// The window the large fixture is read through, which is one sixteenth of it.
const WINDOW: usize = 256 * 1024 * 1024;

/// The value the fixture puts in a given cell.
///
/// The row index, offset by the column. Nothing cleverer, because the failure worth catching here
/// is a window that came back holding the bytes from somewhere else in the file, and a value that
/// is derived from the row says which row turned up instead.
fn cell(column: u64, row: u64) -> i64 {
    i64::try_from(column * 1_000_000_000 + row).expect("the fixture's values all fit")
}

/// The schema for a fixture of this many columns: non-nullable `i64`, one field each.
fn schema(columns: u64) -> Schema {
    Schema::new(
        (0..columns)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, false))
            .collect::<Vec<_>>(),
    )
}

/// The bytes the fixed width decoder reads: two `u64` of header, then column by column.
///
/// Filled in place rather than pushed onto, because the large fixture is four gigabytes and a `Vec`
/// that doubles as it grows would peak at three copies of that. This allocates the whole thing once
/// and writes into it, which is the same reason [`Builder::build_into`] exists.
fn source(rows: u64, columns: u64) -> Vec<u8> {
    let values = usize::try_from(rows * columns).expect("the fixture fits in this host's memory");
    let header = usize::try_from(HEADER).expect("the fixed width header is sixteen bytes");
    let mut out = vec![0u8; header + values * 8];
    out[..8].copy_from_slice(&rows.to_le_bytes());
    out[8..16].copy_from_slice(&columns.to_le_bytes());

    let (slots, rest) = out[header..].as_chunks_mut::<8>();
    debug_assert!(rest.is_empty(), "the buffer holds whole values and no more");
    let mut slots = slots.iter_mut();
    for column in 0..columns {
        for row in 0..rows {
            *slots
                .next()
                .expect("the buffer was sized for exactly these values") =
                cell(column, row).to_le_bytes();
        }
    }
    out
}

/// A container for a fixture of this shape, carrying the decoder that reads it.
fn builder(rows: u64, columns: u64) -> Builder {
    let mut builder = Builder::new("readings", rows);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema(columns)).expect("integer columns always encode"),
    );
    builder.section(SectionKind::Data, source(rows, columns));
    builder.embed_decoder(
        "fixedwidth",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        decoder_module().to_vec(),
    );
    builder
}

/// A file that removes itself, so a gate that fails does not leave four gigabytes behind.
///
/// Declared before the source that reads it in every test here, so it is dropped after that source
/// is closed. Removing a file somebody still has open is fine on Unix and is not on Windows, and
/// this test runs on both.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes `container` to a file of its own under the workspace target directory.
///
/// Under `target` and not under the system temporary directory, which on a lot of Linux machines is
/// a tmpfs. A four gigabyte fixture written to one of those is a four gigabyte fixture held in
/// memory, which is the thing this whole path exists to avoid.
fn write_container(name: &str, builder: &Builder) -> (Scratch, u64) {
    let dir = workspace_root().join("target").join("gate-window");
    std::fs::create_dir_all(&dir).expect("creating the fixture directory");
    let path = dir.join(format!("{name}.iris"));
    let scratch = Scratch(path.clone());

    let file = File::create(&path).expect("creating the fixture");
    let mut out = BufWriter::new(file);
    let len = builder.build_into(&mut out).expect("writing the fixture");
    out.flush().expect("flushing the fixture");
    out.into_inner()
        .expect("the fixture is written")
        .sync_all()
        .expect("the fixture reaches the disk");

    assert_eq!(
        std::fs::metadata(&path)
            .expect("the fixture is there")
            .len(),
        len,
        "the builder said it wrote a different number of bytes than the file holds"
    );
    (scratch, len)
}

/// Every value in a column of the batches, in order.
fn column_values(batches: &[RecordBatch], column: usize) -> Vec<i64> {
    batches
        .iter()
        .flat_map(|batch| {
            batch
                .column(column)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("an Int64 field produces an Int64Array")
                .values()
                .to_vec()
        })
        .collect()
}

/// Where the data section starts, read out of the file with nothing but `std`.
///
/// This is the reference the window is checked against, and it is deliberately not read through a
/// window. It seeks to the trailer, then to the footer, and parses the directory out of the two,
/// which is the metadata path with the source layer taken out from under it.
fn data_section_offset(path: &PathBuf) -> u64 {
    let mut file = File::open(path).expect("the fixture is there");
    let file_len = file.metadata().expect("the fixture is measurable").len();

    let mut trailer = vec![0u8; Placement::TRAILER_LEN];
    file.seek(SeekFrom::Start(
        Placement::trailer_at(file_len).expect("the fixture is a container"),
    ))
    .expect("seeking to the trailer");
    file.read_exact(&mut trailer).expect("reading the trailer");
    let placement = Placement::read(&trailer, file_len).expect("the trailer parses");

    let mut header = [0u8; 16];
    file.rewind().expect("seeking to the header");
    file.read_exact(&mut header).expect("reading the header");

    let mut footer = vec![0u8; placement.footer_len()];
    file.seek(SeekFrom::Start(placement.footer_at()))
        .expect("seeking to the footer");
    file.read_exact(&mut footer).expect("reading the footer");

    let directory = Directory::parse(&header, &footer, placement).expect("the footer parses");
    directory
        .sections()
        .iter()
        .find(|section| section.kind == SectionKind::Data)
        .expect("the fixture has a data section")
        .offset
}

/// The value the file holds for a cell, read with a seek and eight bytes.
fn value_on_disk(path: &PathBuf, data_at: u64, rows: u64, column: u64, row: u64) -> i64 {
    let mut file = File::open(path).expect("the fixture is there");
    file.seek(SeekFrom::Start(
        data_at + HEADER + (column * rows + row) * WIDTH,
    ))
    .expect("seeking to the cell");
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes).expect("reading the cell");
    i64::from_le_bytes(bytes)
}

#[test]
fn a_container_read_through_a_window_decodes_to_what_a_resident_one_does() {
    let builder = builder(ROWS, COLUMNS);
    let resident = builder.build().expect("a container this small always fits");
    let (scratch, len) = write_container("small", &builder);
    assert_eq!(len, resident.len() as u64, "the two writers disagree");

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);

    let source = FileSource::open(&scratch.0).expect("the fixture opens");
    let mut windowed = runtime
        .open_windowed(Box::new(source))
        .expect("the container opens through a window");

    // The dataset says the same things about itself either way. It has to: the metadata came out of
    // the same footer, read through a source instead of out of a slice.
    let dataset = runtime.open(&resident).expect("the container opens");
    assert_eq!(windowed.rows(), dataset.rows());
    assert_eq!(windowed.name(), dataset.name());
    assert_eq!(**windowed.schema(), schema(COLUMNS));

    let through_a_window = windowed.scan().expect("the decoder runs");
    let held = dataset.scan().expect("the decoder runs");

    assert!(
        through_a_window.len() > 1,
        "a {ROWS} row scan at {BATCH_ROWS} rows a batch came back as one batch"
    );
    for column in 0..COLUMNS {
        let at = usize::try_from(column).expect("there are three columns");
        let found = column_values(&through_a_window, at);
        let expected: Vec<i64> = (0..ROWS).map(|row| cell(column, row)).collect();

        // Against the values the fixture was written from first, because a comparison of two runs
        // that are both wrong in the same way passes.
        assert_eq!(found, expected, "column c{column} through the window");
        assert_eq!(
            column_values(&held, at),
            expected,
            "column c{column} held in memory"
        );
    }
}

#[test]
fn a_row_range_through_a_window_is_that_range_and_nothing_else() {
    let (scratch, _) = write_container("range", &builder(ROWS, COLUMNS));
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(
            FileSource::open(&scratch.0).expect("the fixture opens"),
        ))
        .expect("the container opens through a window");

    let batches = windowed.scan_rows(100, 50).expect("the decoder runs");
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(total, 50);
    assert_eq!(
        column_values(&batches, 1),
        (100..150).map(|row| cell(1, row)).collect::<Vec<_>>()
    );
}

/// A windowed dataset can be scanned more than once.
///
/// Worth its own test because the source is handed to the decoder by value and has to come back.
/// A dataset that worked once and then said its source was lost would pass every other test here.
#[test]
fn a_windowed_dataset_survives_the_scan_it_just_ran() {
    let (scratch, _) = write_container("again", &builder(ROWS, COLUMNS));
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(
            FileSource::open(&scratch.0).expect("the fixture opens"),
        ))
        .expect("the container opens through a window");

    let first = column_values(&windowed.scan_rows(0, 10).expect("the decoder runs"), 0);
    let second = column_values(
        &windowed.scan_rows(0, 10).expect("the decoder runs again"),
        0,
    );
    let third = column_values(&windowed.scan_rows(490, 10).expect("and again"), 0);

    assert_eq!(first, second);
    assert_eq!(
        third,
        (490..500).map(|row| cell(0, row)).collect::<Vec<_>>()
    );
}

/// The gate itself: a dataset larger than a wasm32 guest can address, through a 256 MiB window.
///
/// Ignored by default. It writes about four and a third gigabytes to disk and takes a minute or two
/// to fill, which is not something to put in front of every pull request, and hosted runners do not
/// have the disk for it anyway. `.github/workflows/fleet.yml` runs it by name.
///
/// What it proves is the exit condition of the milestone, and it is worth being precise about which
/// part is the interesting one. The rows read at the end of the file sit at byte offsets past four
/// gigabytes. A decoder that narrowed an offset to a pointer would fail on them, a host that mapped
/// the whole file would need four gigabytes of address space in the guest to serve them, and the
/// window holds a sixteenth of the file at a time and serves them anyway. Every value is checked
/// twice: against the formula the fixture was generated from, and against the bytes read straight
/// out of the file with a seek.
#[test]
#[ignore = "writes a four gigabyte fixture, run by name from the fleet workflow"]
fn a_dataset_larger_than_four_gibibytes_reads_through_a_window_a_fraction_of_its_size() {
    /// The ceiling this gate is about.
    const CEILING: u64 = 4 * 1024 * 1024 * 1024;

    let (scratch, len) = write_container("huge", &builder(HUGE_ROWS, 1));
    assert!(
        len > CEILING,
        "the fixture is {len} bytes, which is not larger than the ceiling"
    );

    let file = File::open(&scratch.0).expect("the fixture opens");
    let source = FileSource::with_span(file, WINDOW).expect("the window is reserved");
    let largest = source.largest().expect("a window bounds its requests");

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(source))
        .expect("the container opens through a window");

    assert_eq!(windowed.rows(), HUGE_ROWS);
    assert!(
        windowed.source_bytes() > CEILING,
        "the data section is {} bytes, which is not past the ceiling",
        windowed.source_bytes()
    );
    assert_eq!(windowed.window_bytes(), largest as u64);
    assert!(
        windowed.window_bytes() * 8 < windowed.source_bytes(),
        "a window of {} bytes over {} is not a fraction of it",
        windowed.window_bytes(),
        windowed.source_bytes()
    );

    let data_at = data_section_offset(&scratch.0);

    // The first row whose bytes sit past the ceiling, and the ranges either side of it. Reading the
    // start as well is not padding: a window that never moved would pass a test that only read the
    // end, because it would have been positioned there from the first request.
    let straddling = (CEILING - HEADER) / WIDTH;
    for start in [0, straddling - 32, HUGE_ROWS - 64] {
        let batches = windowed.scan_rows(start, 64).expect("the decoder runs");
        let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
        assert_eq!(total, 64, "the scan from row {start} came back short");

        let found = column_values(&batches, 0);
        for (offset, value) in found.iter().enumerate() {
            let row = start + offset as u64;
            assert_eq!(*value, cell(0, row), "row {row} through the window");
            assert_eq!(
                *value,
                value_on_disk(&scratch.0, data_at, HUGE_ROWS, 0, row),
                "row {row} is not what the file holds at that offset"
            );
        }
    }
}
