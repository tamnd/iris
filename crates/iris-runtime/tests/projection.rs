//! The M5 gate: a projection that reaches storage.
//!
//! Reading three of forty columns should move roughly three fortieths of the data section. If it
//! moves all of it, then the columns nobody asked for were fetched and thrown away, and what the
//! host is doing is filtering a decoded batch rather than pushing anything down. Those two are
//! indistinguishable from the answer and very far apart on a machine where the bytes are somewhere
//! else, so the only way to tell them apart is to count the bytes.
//!
//! The host does not choose which bytes to fetch. The decoder does, by naming ranges, so a
//! projection reaching storage means the decoder was told which columns are wanted, believed it,
//! and asked for less. Every part of that chain is exercised here: the host encodes the projection
//! into the scan request, the decoder reads it back out, and the source counts what arrives.
//!
//! # What counts as the source
//!
//! The tests here measure at a source of their own that counts exactly the bytes it was asked for.
//! That is the decoder's side of the wire rather than the server's, and it is deliberately the
//! cheap check: it runs on every machine, on every commit, with no endpoint and no network.
//!
//! The expensive check is the same claim measured at the far end. `tests/portable.rs` runs the same
//! scan against an S3 compatible server and reads the byte counter out of that server's own
//! metrics, which is the observer with no reason to agree with us. It is ignored in the tree and
//! run by name from the object storage job, for the reason given in that file.
//!
//! The decoder is compiled rather than checked in. See `tests/support/mod.rs`.

use arrow_array::RecordBatch;
use iris_runtime::{Error, Runtime, Traffic};
use iris_source::{Fetch, MemorySource, RangeSource, SourceError};

mod support;

use support::{HEADER, WIDTH, builder, cell, column_values};

/// Rows in the fixture. Enough that a column is worth several batches and the arithmetic below is
/// about columns rather than about a header.
const ROWS: u64 = 20_000;

/// Columns in the fixture, which is the forty in the claim.
const COLUMNS: u64 = 40;

/// The three columns the projected scans ask for.
///
/// Not the first three. A projection of columns zero, one and two is served correctly by a decoder
/// that ignores the projection and stops early, and a projection of the last three is served by one
/// that reads from the end. These are spread out and start past the front, so neither mistake
/// produces the right answer.
const PROJECTED: [u32; 3] = [7, 19, 31];

/// Small enough that a scan is many batches, which is when a source has to move.
const BATCH_ROWS: u64 = 4_096;

/// How far the measured share is allowed to sit from the share that was asked for.
///
/// A tenth of a column. The decoder here asks for exactly the rows of exactly the columns it was
/// given, so the honest answer is exact, and the tolerance is here because the gate is about a
/// pushdown reaching storage rather than about this decoder's request pattern. A decoder that
/// coalesced adjacent requests, or read a little ahead, would still pass and should.
#[expect(
    clippy::cast_precision_loss,
    reason = "a column count in the tens, written as a fraction of itself so the number moves with \
              the fixture rather than being pasted in already worked out"
)]
const TOLERANCE: f64 = 0.1 / COLUMNS as f64;

/// A source that serves from memory and counts what it was asked for.
///
/// [`MemorySource`] reports no traffic at all, which is the honest answer for a buffer somebody
/// else already paid for, and it is exactly the wrong answer here. This one counts every range as
/// though the bytes had to come from somewhere, which is what a scan over an object store looks
/// like with the network taken out of it.
///
/// It keeps the last range it served and does not charge for a request that falls inside it, which
/// is the same one block arrangement [`iris_source::ObjectSource`] has. That is not a refinement:
/// the host asks for every range twice, once to find out whether it is ready and once to take the
/// bytes, so a source that charged for both would report double and every number below would be a
/// fact about the host's borrow checker rather than about the projection.
#[derive(Debug)]
struct Counted {
    inner: MemorySource,
    held: Option<(u64, u64)>,
    requests: u64,
    bytes: u64,
}

impl Counted {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: MemorySource::new(bytes),
            held: None,
            requests: 0,
            bytes: 0,
        }
    }

    /// Whether the block last served covers this range.
    fn covered(&self, at: u64, len: u64) -> bool {
        self.held
            .is_some_and(|(held, span)| at >= held && at + len <= held + span)
    }
}

impl RangeSource for Counted {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        let span = len as u64;
        if !self.covered(at, span) {
            self.requests += 1;
            self.bytes += span;
            self.held = Some((at, span));
        }
        self.inner.range(at, len)
    }

    fn traffic(&self) -> Traffic {
        Traffic {
            requests: self.requests,
            bytes: self.bytes,
        }
    }
}

/// The values the fixture holds in a column, which is what a projected scan has to come back with.
fn expected(column: u32) -> Vec<i64> {
    (0..ROWS).map(|row| cell(u64::from(column), row)).collect()
}

/// How many bytes one column of the fixture takes.
const fn column_bytes() -> u64 {
    ROWS * WIDTH
}

/// What a scan of `columns` whole columns costs, including the decoder's own header.
///
/// The header is in here because every scan pays for it. A scan instantiates the decoder again, and
/// a decoder that has just been instantiated has not read anything yet, so the first thing it does
/// is read the sixteen bytes that say how many rows and columns there are. Sixteen bytes against
/// several megabytes is nothing to a ratio and it is everything to an equality, so it is written
/// down rather than absorbed into a tolerance.
const fn whole_columns(columns: u64) -> u64 {
    HEADER + columns * column_bytes()
}

#[test]
fn a_projected_scan_fetches_the_columns_it_named_and_not_the_others() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    let all = windowed.scan().expect("the decoder reads every column");
    let whole = windowed.last_scan();
    assert_eq!(
        whole.bytes,
        whole_columns(COLUMNS),
        "a scan of every column should move the data section and nothing else"
    );
    assert_eq!(
        all.first().expect("a batch").schema().fields().len(),
        usize::try_from(COLUMNS).expect("forty columns fit in a usize"),
        "an unprojected scan comes back with every column"
    );

    let some = windowed
        .scan_columns(&PROJECTED)
        .expect("the decoder reads three columns");
    let part = windowed.last_scan();

    // The claim, as a ratio rather than as a byte count, because the byte count is a fact about
    // this fixture and the ratio is the thing that is supposed to hold.
    #[expect(
        clippy::cast_precision_loss,
        reason = "two byte counts from one small fixture and two small column counts, divided to \
                  get a ratio that is compared against a tolerance far larger than anything this \
                  can lose"
    )]
    let (share, asked) = (
        part.bytes as f64 / whole.bytes as f64,
        PROJECTED.len() as f64 / COLUMNS as f64,
    );
    assert!(
        (share - asked).abs() < TOLERANCE,
        "three of {COLUMNS} columns moved {} bytes of {}, which is {share:.4} of the data section \
         and not the {asked:.4} that was asked for",
        part.bytes,
        whole.bytes
    );

    // And the answer. A scan that moved the right number of bytes and came back with the wrong
    // columns would be a worse bug than one that moved too many.
    for (at, &column) in PROJECTED.iter().enumerate() {
        assert_eq!(
            column_values(&some, at),
            expected(column),
            "the batch's field {at} is not column {column} of the fixture"
        );
    }
}

#[test]
fn the_batches_of_a_projected_scan_carry_the_columns_that_were_asked_for() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    let batches = windowed
        .scan_columns(&PROJECTED)
        .expect("the decoder reads three columns");
    let schema = batches.first().expect("a batch").schema();
    assert_eq!(schema.fields().len(), PROJECTED.len());
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["c7", "c19", "c31"]);
}

#[test]
fn a_projection_is_served_in_the_order_it_was_written() {
    // Backwards and with a repeat, which is a projection somebody would write by accident and one
    // that has an unambiguous right answer. Column nineteen twice means two fields holding the same
    // values, and it costs twice: the decoder is not asked to notice.
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    let batches = windowed
        .scan_columns(&[31, 19, 7, 19])
        .expect("the decoder reads four fields");
    assert_eq!(column_values(&batches, 0), expected(31));
    assert_eq!(column_values(&batches, 1), expected(19));
    assert_eq!(column_values(&batches, 2), expected(7));
    assert_eq!(column_values(&batches, 3), expected(19));
    assert_eq!(windowed.last_scan().bytes, whole_columns(4));
}

#[test]
fn a_row_range_and_a_projection_narrow_a_scan_in_both_directions() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    let batches = windowed
        .scan_rows_columns(1_000, 50, &PROJECTED)
        .expect("the decoder reads fifty rows of three columns");
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows, 50);
    assert_eq!(
        column_values(&batches, 1),
        (1_000..1_050)
            .map(|row| cell(u64::from(PROJECTED[1]), row))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        windowed.last_scan().bytes,
        HEADER + 50 * WIDTH * PROJECTED.len() as u64,
        "fifty rows of three columns is three ranges of fifty values"
    );
}

#[test]
fn a_projection_naming_a_column_the_dataset_does_not_have_is_refused() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new().expect("the engine builds");
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    // Forty columns are numbered zero to thirty nine, so this is the off by one the error exists to
    // name. It is refused by the host before the decoder is asked for anything, which is why the
    // scan reports no traffic at all.
    let error = windowed
        .scan_columns(&[7, 40])
        .expect_err("column forty is not one of forty columns");
    assert!(
        matches!(
            error,
            Error::Projection {
                column: 40,
                fields: 40
            }
        ),
        "{error}"
    );
}

#[test]
fn a_resident_scan_takes_a_projection_and_reports_that_it_cost_nothing() {
    // The resident path answers the same question and moves nothing either way, because the whole
    // container was paid for before the dataset existed. That is not a reason for the projection to
    // be ignored there: the decoder still does a fortieth of the work, and a caller that writes one
    // scan for both paths should get the same columns back from both.
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let dataset = runtime.open(&bytes).expect("the container opens");

    let batches = dataset
        .scan_columns(&PROJECTED)
        .expect("the decoder reads three columns");
    for (at, &column) in PROJECTED.iter().enumerate() {
        assert_eq!(column_values(&batches, at), expected(column));
    }
    assert_eq!(
        dataset.last_scan(),
        Traffic::NONE,
        "a scan over bytes that are already here does not fetch anything"
    );
}

#[test]
fn the_header_is_the_only_thing_a_scan_reads_that_is_not_a_column() {
    // Written down because every byte count above is a multiple of a column plus a header, and that
    // only means anything if the header really is the only other thing a scan reads. Opening reads
    // the trailer, the container header, the footer and the decoder module, and none of that is a
    // scan or is counted as one.
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this size fits");
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let mut windowed = runtime
        .open_windowed(Box::new(Counted::new(bytes)))
        .expect("the container opens over a counting source");

    let opening = windowed.traffic();
    assert!(
        opening.bytes >= HEADER,
        "opening has to have read at least the decoder's own header"
    );
    windowed.scan_columns(&[0]).expect("the decoder runs");
    assert_eq!(
        windowed.last_scan().bytes,
        whole_columns(1),
        "a scan of one column reads that column, its own header, and nothing else"
    );
}
