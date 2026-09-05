//! The M1 gate: a container on disk, through a real WebAssembly decoder, out as Arrow.
//!
//! Everything else in this workspace tests one hop. This tests the whole path, and it is the test
//! that says whether the architecture works at all: a dataset that carries its own decoder, a host
//! that has never seen the encoding, and an answer that agrees with arrow-rs value by value.
//!
//! The decoders are compiled rather than checked in. See `tests/support/mod.rs` for why, and for
//! the locking that makes a nested cargo safe to run from several test processes at once.

use std::time::{Duration, Instant};

use arrow_array::{Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet};
use iris_format::{Builder, Digest, SchemaEncoding, SectionKind};
use iris_runtime::{Error, Policy, Resolve, Runtime, Untrusted, schema_to_ipc};

mod support;

use support::{decoder_module, looping_module};

/// How many rows the fixture holds.
const ROWS: u64 = 500;

/// How many columns it holds.
const COLUMNS: u64 = 3;

/// Small enough that the scan comes back in several batches, which is the case worth testing.
const BATCH_ROWS: u64 = 128;

/// The value the fixture puts in a given cell.
fn cell(column: u64, row: u64) -> i64 {
    i64::try_from(column * 1000 + row).expect("the fixture's values are all small")
}

/// The schema the fixture declares: three non-nullable `i64` columns.
fn schema() -> Schema {
    Schema::new(
        (0..COLUMNS)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, false))
            .collect::<Vec<_>>(),
    )
}

/// The bytes the fixed width decoder reads: two `u64` of header, then column by column.
fn source() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ROWS.to_le_bytes());
    out.extend_from_slice(&COLUMNS.to_le_bytes());
    for column in 0..COLUMNS {
        for row in 0..ROWS {
            out.extend_from_slice(&cell(column, row).to_le_bytes());
        }
    }
    out
}

/// A whole container: schema, data, and the decoder that reads it.
fn container() -> Vec<u8> {
    container_at_abi((ABI_MAJOR, ABI_MINOR))
}

/// The same container, declaring whatever ABI it is handed.
///
/// The module is the real one either way. What changes is what the container says about it, which is
/// the thing a host has to act on: it cannot know whether a module is honest about its ABI without
/// running it, and running it is exactly what it must not do first.
fn container_at_abi(abi: (u16, u16)) -> Vec<u8> {
    let mut builder = Builder::new("readings", ROWS);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema()).expect("three integer columns always encode"),
    );
    builder.section(SectionKind::Data, source());
    builder.embed_decoder(
        "fixedwidth",
        abi,
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        decoder_module().to_vec(),
    );
    builder.build().expect("a container this small always fits")
}

/// The same dataset, carrying the decoder that never returns.
///
/// Nothing about this container is malformed. It parses, its digest is right, its schema is the one
/// every other test uses, and the decoder in it does exactly what a decoder is supposed to do right
/// up until the scan. That is the point: the only thing wrong with it is the thing metering catches.
fn container_that_never_returns() -> Vec<u8> {
    let mut builder = Builder::new("readings", ROWS);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema()).expect("three integer columns always encode"),
    );
    builder.section(SectionKind::Data, source());
    builder.embed_decoder(
        "looping",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new(),
        looping_module().to_vec(),
    );
    builder.build().expect("a container this small always fits")
}

/// The same dataset, with the decoder named rather than carried.
///
/// The module is not in the file at all. What is in the file is its digest, which is what a host
/// that goes and finds it has to check the answer against.
fn container_naming_its_decoder() -> Vec<u8> {
    let mut builder = Builder::new("readings", ROWS);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema()).expect("three integer columns always encode"),
    );
    builder.section(SectionKind::Data, source());
    builder.external_decoder(
        "fixedwidth",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        Digest::of(decoder_module()),
    );
    builder.build().expect("a container this small always fits")
}

/// A host that keeps one decoder and hands it to anybody who asks.
///
/// A real one would look in a directory or call a registry. This one is the same shape with the
/// finding part removed, which is all this test needs: what matters is that the bytes it returns go
/// through the same hash as an embedded module.
#[derive(Debug)]
struct OneModule(Vec<u8>);

impl Resolve for OneModule {
    fn resolve(&self, _decoder: &iris_format::DecoderRef<'_>) -> Option<Vec<u8>> {
        Some(self.0.clone())
    }
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

#[test]
fn a_container_decodes_to_the_same_values_arrow_would_have_produced() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let dataset = runtime.open(&bytes).expect("the container opens");

    assert_eq!(dataset.rows(), ROWS);
    assert_eq!(dataset.name(), "readings");
    assert_eq!(**dataset.schema(), schema());

    let batches = dataset.scan().expect("the decoder runs");

    // The point of the small batch size. A decoder that ignored it and emitted one batch would
    // still produce the right values, and that is a different thing from working.
    assert!(
        batches.len() > 1,
        "a {ROWS} row scan at {BATCH_ROWS} rows a batch came back as {} batch(es)",
        batches.len()
    );

    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(u64::try_from(total).expect("five hundred rows"), ROWS);

    // Value by value against what arrow-rs would have built from the same numbers. The whole array
    // comparison at the end is the one that matters, and the loop before it is there because a whole
    // array comparison that fails says "not equal" and nothing else, while the first row where two
    // orderings diverge is the thing a reader actually needs.
    for column in 0..COLUMNS {
        let expected: Vec<i64> = (0..ROWS).map(|row| cell(column, row)).collect();
        let found = column_values(
            &batches,
            usize::try_from(column).expect("there are three columns"),
        );
        assert_eq!(
            found.len(),
            expected.len(),
            "column c{column} has the wrong length"
        );
        for (row, (f, e)) in found.iter().zip(&expected).enumerate() {
            assert_eq!(f, e, "column c{column} row {row}");
        }
        assert_eq!(
            Int64Array::from(found),
            Int64Array::from(expected),
            "column c{column} does not match the array arrow-rs builds from the same values"
        );
    }

    // Nothing in the fixture is null, so anything that says otherwise is the null bitmap being
    // invented rather than read.
    for batch in &batches {
        for column in batch.columns() {
            assert_eq!(column.null_count(), 0);
        }
    }
}

#[test]
fn a_row_range_comes_back_as_that_range_and_nothing_else() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);
    let dataset = runtime.open(&bytes).expect("the container opens");

    let batches = dataset.scan_rows(100, 50).expect("the decoder runs");
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(total, 50);

    let found = column_values(&batches, 1);
    let expected: Vec<i64> = (100..150).map(|row| cell(1, row)).collect();
    assert_eq!(found, expected);
}

#[test]
fn a_range_that_starts_past_the_end_is_empty_rather_than_an_error() {
    let bytes = container();
    let runtime = Runtime::new().expect("the engine builds");
    let dataset = runtime.open(&bytes).expect("the container opens");

    let batches = dataset
        .scan_rows(ROWS + 1000, 10)
        .expect("asking for rows that are not there is a real query with an empty answer");
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(total, 0);
}

#[test]
fn a_dataset_from_a_later_abi_is_refused_with_something_worth_reading() {
    let bytes = container_at_abi((ABI_MAJOR + 1, 0));
    let runtime = Runtime::new().expect("the engine builds");
    let err = runtime
        .open(&bytes)
        .expect_err("a decoder from a later major ABI cannot run on this host");

    assert!(
        matches!(err, Error::Abi { .. }),
        "a later ABI should be refused as an ABI problem, and it said: {err}"
    );

    // The three things somebody holding a dataset they cannot open needs, asserted one at a time so
    // that a message which drops one of them fails on the one it dropped.
    let message = err.to_string();

    // Which host would read it, and which one this is.
    assert!(
        message.contains(&format!("{}.0", ABI_MAJOR + 1)),
        "the message does not name the ABI the dataset needs: {message}"
    );
    assert!(
        message.contains(&format!("{ABI_MAJOR}.{ABI_MINOR}")),
        "the message does not name the ABI this host speaks: {message}"
    );

    // Which decoder to go and find. The digest is the identity, the name is the courtesy.
    assert!(
        message.contains(&Digest::of(decoder_module()).to_string()),
        "the message does not name the decoder digest: {message}"
    );
    assert!(
        message.contains("fixedwidth"),
        "the message does not name the decoder: {message}"
    );

    // Whether this is even the dataset they were after.
    assert!(
        message.contains("c0: Int64"),
        "the message does not name the schema: {message}"
    );
    assert!(
        message.contains("c2: Int64"),
        "the message names only part of the schema: {message}"
    );
}

/// The M2 gate for verifying a decoder before compiling it.
///
/// One byte, one bit, inside the module and nowhere else. The rest of the file is exactly the file
/// that works, which is what makes this the interesting case rather than a corrupt file test: the
/// container's root digest covers the header and the footer, so a bit changed inside a section
/// parses without complaint. If the digest were stored and not checked, this container would open,
/// compile a module nobody wrote and run it. It is refused instead, and the refusal carries both
/// digests: the one to go and look for, and the one that turned up.
#[test]
fn one_flipped_bit_in_the_decoder_is_refused_with_both_digests() {
    let module = decoder_module();
    let mut bytes = container();
    let at = bytes
        .windows(module.len())
        .position(|window| window == module)
        .expect("the builder wrote the module into the file");
    bytes[at + module.len() / 2] ^= 1;

    let runtime = Runtime::new().expect("the engine builds");
    let error = runtime
        .open(&bytes)
        .expect_err("a container carrying a module nobody wrote opened");

    let Error::Trust(Untrusted::Digest { expected, found }) = &error else {
        panic!("a flipped bit in the decoder should be refused by digest, and said: {error}");
    };
    assert_eq!(
        *expected,
        Digest::of(module),
        "the container should still name the module that was built"
    );
    assert_ne!(found, expected, "the flipped bit did not change the hash");

    let message = error.to_string();
    assert!(
        message.contains(&expected.to_string()),
        "the message does not say which module was expected: {message}"
    );
    assert!(
        message.contains(&found.to_string()),
        "the message does not say what arrived instead: {message}"
    );
}

/// The M2 gate for embedded decoders being the default.
///
/// A dataset that names a decoder by URI is asking this host to go and get something and then run
/// it. Out of the box it does not, and the refusal says what would have allowed it, because the
/// alternative is an operator reading the source of a crate to find out.
#[test]
fn a_decoder_that_is_not_in_the_container_does_not_run_by_default() {
    let bytes = container_naming_its_decoder();
    let runtime = Runtime::new().expect("the engine builds");

    let error = runtime
        .open(&bytes)
        .expect_err("a dataset that names a decoder somewhere else opened on its own say so");

    let Error::Trust(Untrusted::External { name }) = &error else {
        panic!("a referenced decoder should fail closed, and said: {error}");
    };
    assert_eq!(name, "fixedwidth");

    let message = error.to_string();
    assert!(
        message.contains("Policy::with_external_decoders_resolved_by"),
        "the refusal does not name the setting that would allow this: {message}"
    );
}

/// The other half of it: a host that opted in gets the dataset, and the bytes its resolver found
/// went through the same hash the embedded case goes through.
#[test]
fn a_host_that_opted_in_reads_the_same_dataset() {
    let bytes = container_naming_its_decoder();
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS)
        .with_decoder_policy(Policy::with_external_decoders_resolved_by(OneModule(
            decoder_module().to_vec(),
        )));

    let dataset = runtime
        .open(&bytes)
        .expect("the resolver found the decoder");
    let batches = dataset.scan().expect("the decoder runs");
    assert_eq!(
        column_values(&batches, 0),
        (0..ROWS).map(|r| cell(0, r)).collect::<Vec<_>>()
    );
}

/// And a resolver that comes back with something else is caught by the digest rather than compiled.
#[test]
fn a_resolver_that_finds_the_wrong_module_is_still_checked() {
    let bytes = container_naming_its_decoder();
    let mut wrong = decoder_module().to_vec();
    let middle = wrong.len() / 2;
    wrong[middle] ^= 1;

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_decoder_policy(Policy::with_external_decoders_resolved_by(OneModule(wrong)));

    let error = runtime
        .open(&bytes)
        .expect_err("a fetched module nobody checked was compiled");
    assert!(
        matches!(error, Error::Trust(Untrusted::Digest { .. })),
        "a fetched module should be checked like any other, and this said: {error}"
    );
}

/// A decoder that never returns costs the query it was running, and says which decoder it was.
///
/// This is the whole reason metering is a gate. Everything else in this file is about a host
/// refusing bytes it does not trust, and none of it helps against a module that is exactly what the
/// container says it is and simply does not stop. The host has to be able to take its thread back.
#[test]
fn a_decoder_that_never_returns_is_stopped_and_named() {
    let bytes = container_that_never_returns();

    // Short enough that a failing test is over quickly. Nothing else in this file needs a deadline
    // at all, because the default is ten seconds and every honest decoder here is done in
    // microseconds.
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_decoder_deadline(Duration::from_millis(250));
    let dataset = runtime
        .open(&bytes)
        .expect("the container is well formed, that is the point of it");

    let started = Instant::now();
    let error = dataset.scan().expect_err("the decoder does not return");
    let waited = started.elapsed();

    assert!(
        matches!(error, Error::Vm(_)),
        "a decoder that never returns should come back from the vm, and this said: {error}"
    );

    let message = error.to_string();
    let digest = Digest::of(looping_module()).to_string();
    assert!(
        message.contains(&digest),
        "whoever reads this has to know which decoder to go and look at: {message}"
    );
    assert!(
        message.contains("did not come back"),
        "a decoder that ran away is not the same as one that broke: {message}"
    );

    assert!(
        waited < Duration::from_secs(5),
        "the deadline was 250ms and the scan took {waited:?}"
    );
}
