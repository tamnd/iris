//! The M5 gate: idempotence and statelessness, checked against every decoder in the tree.
//!
//! Two properties a host relies on the moment it stops scanning a dataset with one thread. If it
//! splits a scan into tuple ranges and hands them to a pool, then a range that has to be retried
//! because a worker died has to come back the same, and the ranges have to be allowed to finish in
//! whatever order the pool finishes them in. Neither of those is something a host can check at run
//! time without doing the work twice, so it is checked here instead, once, against every decoder
//! that exists.
//!
//! **Idempotence.** A range of rows requested twice returns byte identical data.
//!
//! **Statelessness.** Ranges requested in a shuffled order are, one for one, what the same ranges
//! requested in sequence produce.
//!
//! # Why this is a real check and not a tautology
//!
//! It is worth being clear about which part of this the host already guarantees, because the answer
//! is most of it and the part that is left is where a bug would actually live.
//!
//! A decoder gets a fresh instance per scan. `iris-runtime` compiles the module once and
//! instantiates it per call, so a decoder has a new linear memory every time it is asked for rows
//! and there is nowhere for it to keep anything between two calls. A WebAssembly guest with no
//! imports beyond the one this host gives it also has no clock, no entropy and no threads, so
//! within an instance its output is a function of the request and the bytes it was served. That is
//! a strong structural position and it is why these two properties are stated as gates rather than
//! as hopes.
//!
//! What is not structural is the bytes it was served. On the resident path the source is a buffer
//! that does not move. On the windowed path it is a source with a position: a window that slides, a
//! block that is kept or dropped, counters that go up. `iris_source` promises that a range does not
//! depend on which ranges came before it and that a ready range stays ready with the same bytes,
//! and its own conformance suite checks a source against that in isolation. These tests check the
//! same two promises through the whole stack, which is the only place where a moving window, a
//! re-instantiated decoder and an assembled batch are all in the picture at once.
//!
//! So the harnesses are pointed at the seam rather than at the decoder. That is also why the two
//! tests that show the harnesses have teeth work by putting a source that drifts underneath an
//! honest decoder, rather than by writing a dishonest decoder: a dishonest decoder of that kind
//! cannot be written against this host, and a check that can only fail in a way nobody can produce
//! is a check nobody should trust.
//!
//! # Byte identical, meaning byte identical
//!
//! Batches are compared by serialising them to an Arrow IPC stream and comparing the bytes. Arrow's
//! own equality compares values, which would call two results equal when one carried a validity
//! buffer the other did not, or when the same numbers arrived split across different batches. The
//! claim is about bytes, so the comparison is about bytes, and it is made by a writer nobody here
//! wrote.
//!
//! The decoders are compiled rather than checked in. See `tests/support/mod.rs`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::RecordBatch;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::SchemaRef;
use iris_runtime::{Dataset, Runtime, Windowed};
use iris_source::{Fetch, FileSource, RangeSource, SourceError, Traffic};

mod support;

use support::{HEADER, builder, cell, column_values, flat_builder, write_container};

/// Rows in the fixed width fixture.
///
/// Not a multiple of the window below, so the last window is a short one. A harness that only ever
/// sees windows of one size is not checking the case where the last piece of a split scan is the
/// odd one out, which is every real split scan.
const ROWS: u64 = 5_000;

/// Columns in the fixed width fixture. More than one, so a column that arrived from the wrong
/// offset is wrong by a whole column rather than by nothing.
const COLUMNS: u64 = 3;

/// Rows in the flat fixture, which is what the passthrough decoder will serve in one call.
///
/// That decoder caps a scan at this many rows of its own accord, so a fixture of exactly this size
/// is one where a scan of everything is answerable. The harness never asks it for more than it has,
/// because a decoder is not required to clamp an impossible request and that is a different gate.
const FLAT_ROWS: u64 = 1_024;

/// The window a split scan is cut into for the fixed width fixture.
const WINDOW: u64 = 512;

/// The window a split scan is cut into for the flat fixture, which divides it evenly.
const FLAT_WINDOW: u64 = 128;

/// Small enough that a scan comes back in several batches, which is when a window has to move.
const BATCH_ROWS: u64 = 128;

/// Where this gate keeps its fixtures.
const SCRATCH: &str = "gate-harness";

/// The seed the shuffle is drawn from, written down so a failure is reproducible.
const SEED: u64 = 0x5152_5354_5556_5758;

/// Something a scan can be asked of, so a harness does not care which path it is driving.
///
/// The two paths have different shapes for a reason that matters: a resident dataset scans through
/// a shared reference and a windowed one needs a unique one, because a source is a position and not
/// only a place. The harnesses want both, so they take the stricter of the two, and the resident
/// path gives up nothing by meeting it.
///
/// The methods are not named after the inherent ones they call. A trait method and an inherent
/// method with the same name on the same type compile to whichever one the resolver prefers, which
/// is a thing to know rather than a thing to rely on in a file whose whole subject is two functions
/// returning the same answer.
trait Scanner {
    /// The schema the batches carry, which the comparison needs even when there are no batches.
    fn scanned_schema(&self) -> SchemaRef;

    /// Reads a range of rows, or says why it could not.
    ///
    /// The error is a string rather than the runtime's own type because a harness reports a
    /// finding, and a scan that failed on the replay when it succeeded the first time is exactly
    /// the finding rather than something to unwrap through.
    fn read_rows(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>, String>;
}

impl Scanner for Dataset<'_> {
    fn scanned_schema(&self) -> SchemaRef {
        Arc::clone(self.schema())
    }

    fn read_rows(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>, String> {
        self.scan_rows(start, count).map_err(|err| err.to_string())
    }
}

impl Scanner for Windowed {
    fn scanned_schema(&self) -> SchemaRef {
        Arc::clone(self.schema())
    }

    fn read_rows(&mut self, start: u64, count: u64) -> Result<Vec<RecordBatch>, String> {
        self.scan_rows(start, count).map_err(|err| err.to_string())
    }
}

/// The batches as an Arrow IPC stream, which is the byte identity the gate is about.
fn fingerprint(schema: &SchemaRef, batches: &[RecordBatch]) -> Vec<u8> {
    let mut writer =
        StreamWriter::try_new(Vec::new(), schema).expect("a schema this host opened encodes");
    for batch in batches {
        writer
            .write(batch)
            .expect("a batch this host assembled encodes");
    }
    writer.finish().expect("the stream closes");
    writer.into_inner().expect("the stream owns its buffer")
}

/// The ranges the idempotence harness replays, for a dataset of `rows` rows.
///
/// All of them are inside the dataset. The offsets are chosen so that a decoder ignoring
/// `row_start` fails: reading from the front and reading from row one produce different data unless
/// the values repeat, and they do not.
fn ranges(rows: u64, cap: u64) -> Vec<(u64, u64)> {
    let span = rows.min(cap);
    vec![
        (0, 1),
        (rows / 2, 1),
        (rows - 1, 1),
        (0, span),
        (1, span - 1),
        (rows / 3, span / 2),
        (rows, 0),
    ]
}

/// Asks for each range twice, and then for all of them again once the others have run.
///
/// Two failures rather than one. A source that is wrong on a replay is wrong immediately. A source
/// that is wrong because something else moved its window is only wrong after something else has
/// run, and a harness that never interleaves would never see the second one.
fn replayed_twice(scanner: &mut dyn Scanner, spans: &[(u64, u64)]) -> Result<(), String> {
    let schema = scanner.scanned_schema();
    let mut first = Vec::with_capacity(spans.len());

    for &(start, count) in spans {
        let once = fingerprint(&schema, &scanner.read_rows(start, count)?);
        let again = fingerprint(&schema, &scanner.read_rows(start, count)?);
        if let Some(at) = differs(&once, &again) {
            return Err(format!(
                "rows {start} to {} came back differently on the replay, {at}",
                start + count
            ));
        }
        first.push(once);
    }

    for (&(start, count), expected) in spans.iter().zip(&first) {
        let now = fingerprint(&schema, &scanner.read_rows(start, count)?);
        if let Some(at) = differs(expected, &now) {
            return Err(format!(
                "rows {start} to {} came back differently after the other {} ranges had been read, \
                 {at}",
                start + count,
                spans.len() - 1
            ));
        }
    }
    Ok(())
}

/// Where two encoded results stop agreeing, said in a way a reader can act on.
///
/// A length is not enough on its own. The interesting failure here changes values and not sizes, so
/// two results that differ are usually the same number of bytes long, and a message that reports
/// only the length would print the same number twice and look like a bug in the message.
fn differs(a: &[u8], b: &[u8]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} bytes against {}", a.len(), b.len()));
    }
    let at = a.iter().zip(b).position(|(x, y)| x != y)?;
    Some(format!("first at byte {at} of {}", a.len()))
}

/// Reads the dataset in windows, in order and then shuffled, and compares the two window by window.
///
/// Window by window rather than as one concatenation, so a decoder that returned the right values
/// split into different batches when the order changed is a failure here. It should be: a host that
/// splits a scan hands those batches on as they are.
fn shuffled_windows(scanner: &mut dyn Scanner, rows: u64, window: u64) -> Result<(), String> {
    let schema = scanner.scanned_schema();
    let windows: Vec<(u64, u64)> = (0..rows.div_ceil(window))
        .map(|w| {
            let start = w * window;
            (start, window.min(rows - start))
        })
        .collect();

    let mut sequential = Vec::with_capacity(windows.len());
    for &(start, count) in &windows {
        sequential.push(fingerprint(&schema, &scanner.read_rows(start, count)?));
    }

    let mut order: Vec<usize> = (0..windows.len()).collect();
    shuffle(&mut order, SEED);
    if order.iter().enumerate().all(|(at, &which)| at == which) {
        return Err("the shuffle produced the order it started in, so this proved nothing".into());
    }

    let mut out_of_order = vec![Vec::new(); windows.len()];
    for &which in &order {
        let (start, count) = windows[which];
        out_of_order[which] = fingerprint(&schema, &scanner.read_rows(start, count)?);
    }

    for (which, (found, expected)) in out_of_order.iter().zip(&sequential).enumerate() {
        if found != expected {
            let (start, count) = windows[which];
            return Err(format!(
                "window {which}, rows {start} to {}, differs from what it was in sequence",
                start + count
            ));
        }
    }
    Ok(())
}

/// Fisher-Yates over a fixed seed, so a failing run is a run somebody else can have.
///
/// The generator is splitmix64, which is four lines and is not being asked to be good. What the
/// shuffle has to be is the same every time and not the identity, and the caller checks the second
/// of those rather than assuming it.
fn shuffle(order: &mut [usize], seed: u64) {
    let mut state = seed;
    let mut next = move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    for at in (1..order.len()).rev() {
        let bound = u64::try_from(at).expect("a list this short fits in a u64") + 1;
        let pick = usize::try_from(next() % bound).expect("a value under the bound fits");
        order.swap(at, pick);
    }
}

/// Both harnesses against a dataset, resident and through a window.
///
/// One function rather than four tests because building the fixture and compiling the decoder is
/// most of the cost, and a failure names which of the four it was.
fn both_ways(name: &str, container: &[u8], path: &Path, rows: u64, window: u64) {
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let spans = ranges(rows, window * 2);

    let mut dataset = runtime.open(container).expect("the container opens");
    replayed_twice(&mut dataset, &spans)
        .unwrap_or_else(|why| panic!("{name} resident is not idempotent: {why}"));
    shuffled_windows(&mut dataset, rows, window)
        .unwrap_or_else(|why| panic!("{name} resident is not stateless: {why}"));

    let mut windowed = runtime
        .open_windowed(Box::new(FileSource::open(path).expect("the fixture opens")))
        .expect("the container opens through a window");
    replayed_twice(&mut windowed, &spans)
        .unwrap_or_else(|why| panic!("{name} through a window is not idempotent: {why}"));
    shuffled_windows(&mut windowed, rows, window)
        .unwrap_or_else(|why| panic!("{name} through a window is not stateless: {why}"));
}

#[test]
fn the_fixed_width_decoder_is_idempotent_and_stateless() {
    let builder = builder(ROWS, COLUMNS);
    let container = builder.build().expect("a container this small always fits");
    let (scratch, _) = write_container(SCRATCH, "fixedwidth", &builder);
    both_ways("fixedwidth", &container, &scratch.0, ROWS, WINDOW);
}

#[test]
fn the_passthrough_decoder_is_idempotent_and_stateless() {
    let builder = flat_builder(FLAT_ROWS);
    let container = builder.build().expect("a container this small always fits");
    let (scratch, _) = write_container(SCRATCH, "passthrough", &builder);
    both_ways(
        "passthrough",
        &container,
        &scratch.0,
        FLAT_ROWS,
        FLAT_WINDOW,
    );
}

/// A dataset read a window at a time is the dataset, and not merely self consistent.
///
/// The two harnesses compare a run against another run, which two runs that are both wrong in the
/// same way would pass. This one compares against the values the fixture was written from, so the
/// harnesses are anchored to something outside themselves.
#[test]
fn the_values_the_harness_compares_are_the_values_the_fixture_holds() {
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let container = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this small always fits");
    let dataset = runtime.open(&container).expect("the container opens");

    let step = usize::try_from(WINDOW).expect("the window fits in this host's memory");
    let mut found = Vec::new();
    for start in (0..ROWS).step_by(step) {
        let batches = dataset
            .scan_rows(start, WINDOW.min(ROWS - start))
            .expect("the decoder runs");
        found.extend(column_values(&batches, 1));
    }
    let expected: Vec<i64> = (0..ROWS).map(|row| cell(1, row)).collect();
    assert_eq!(found, expected, "column c1 read a window at a time");
}

/// A source that answers the same question differently depending on when it was asked.
///
/// This is the negation of two of the promises in `iris_source`: that a ready range stays ready with
/// the same bytes, and that a range does not depend on which ranges came before it. It exists so the
/// harnesses can be shown to fail on something, because a check that has never been seen to fail is
/// a check nobody has any reason to believe.
///
/// It is honest until it is told not to be. The container is opened through it, and a container
/// whose footer changes underneath the parser does not fail these harnesses, it fails to open,
/// which is a different thing and would exercise nothing. So the test opens the dataset first and
/// starts the drift afterwards, at which point the only bytes anybody asks for are the decoder's.
#[derive(Debug)]
struct Drifting {
    bytes: Vec<u8>,
    held: Vec<u8>,
    dishonest: Arc<AtomicBool>,
    served: u64,
    fetched: u64,
}

impl Drifting {
    fn new(bytes: Vec<u8>, dishonest: Arc<AtomicBool>) -> Self {
        Self {
            bytes,
            held: Vec::new(),
            dishonest,
            served: 0,
            fetched: 0,
        }
    }
}

impl RangeSource for Drifting {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).expect("a fixture this small fits in a u64")
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        let wanted = u64::try_from(len).expect("a request this small fits in a u64");
        let start = usize::try_from(at).expect("a fixture this small is addressable");
        let end = start
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(SourceError::OutOfBounds {
                at,
                end: at.saturating_add(wanted),
                len: self.len(),
            })?;

        self.held.clear();
        self.held.extend_from_slice(&self.bytes[start..end]);
        self.served = self.served.wrapping_add(1);
        self.fetched = self.fetched.saturating_add(wanted);

        // The top byte of the last value in the range, so what comes back is a different number and
        // not a malformed anything. Every column in these fixtures is a non-nullable eight byte
        // integer, so there is no offset and no validity bit to land on, and no way for this to be
        // caught as a shape error instead of as the drift it is. The low bit is forced on so that
        // the counter reaching a multiple of two hundred and fifty six is not a moment of honesty.
        //
        // Ranges no longer than a header are left alone, and that is the whole subtlety in this
        // type. A decoder is instantiated afresh for every scan, so it reads its header again every
        // time, and a header that drifts makes the decoder refuse the scan. Refusing is caught by
        // any check at all. What these tests have to show is that the harnesses notice a decode
        // that succeeded and returned something else, which is the failure that would otherwise go
        // out to a caller as data.
        if self.dishonest.load(Ordering::Relaxed)
            && wanted > HEADER
            && let Some(last) = self.held.last_mut()
        {
            *last ^= self.served.to_le_bytes()[0] | 1;
        }
        Ok(Fetch::Ready(&self.held))
    }

    fn traffic(&self) -> Traffic {
        Traffic {
            requests: self.served,
            bytes: self.fetched,
        }
    }
}

/// Opens a container through a source that starts drifting once the container is open.
fn drifting_dataset() -> Windowed {
    let container = builder(ROWS, COLUMNS)
        .build()
        .expect("a container this small always fits");
    let dishonest = Arc::new(AtomicBool::new(false));
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let windowed = runtime
        .open_windowed(Box::new(Drifting::new(container, Arc::clone(&dishonest))))
        .expect("the container opens while the source is still honest");
    dishonest.store(true, Ordering::Relaxed);
    windowed
}

#[test]
fn the_idempotence_harness_fails_on_a_source_that_answers_differently() {
    let mut windowed = drifting_dataset();
    let why = replayed_twice(&mut windowed, &ranges(ROWS, WINDOW * 2))
        .expect_err("a source that answers differently is not idempotent");
    assert!(
        why.contains("came back differently"),
        "the harness failed for a reason it does not name: {why}"
    );
}

#[test]
fn the_statelessness_harness_fails_on_a_source_that_depends_on_what_came_before() {
    let mut windowed = drifting_dataset();
    let why = shuffled_windows(&mut windowed, ROWS, WINDOW)
        .expect_err("a source that depends on request order is not stateless");
    assert!(
        why.contains("differs from what it was in sequence"),
        "the harness failed for a reason it does not name: {why}"
    );
}
