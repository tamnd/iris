//! The M1 gate: a container on disk, through a real WebAssembly decoder, out as Arrow.
//!
//! Everything else in this workspace tests one hop. This tests the whole path, and it is the test
//! that says whether the architecture works at all: a dataset that carries its own decoder, a host
//! that has never seen the encoding, and an answer that agrees with arrow-rs value by value.
//!
//! # Why it builds the decoder rather than checking one in
//!
//! A committed `.wasm` fixture is a binary nobody reads, built by a toolchain nobody remembers,
//! that keeps passing after the source it came from has stopped matching it. The interesting
//! failure here is the ABI drifting away from the SDK, and a stale fixture is precisely the thing
//! that hides it. So the decoder is compiled from `crates/iris-decoder/examples/fixedwidth.rs`
//! every time this test runs.
//!
//! The cost is that running the test suite needs the wasm32 target installed. `rust-toolchain.toml`
//! asks for it, so rustup puts it there without anybody thinking about it, and the failure message
//! says what to do if somebody is running a toolchain that ignored the file.
//!
//! The nested cargo gets its own target directory. Cargo's lock is per target directory, so
//! building into the one the outer cargo is holding would deadlock rather than fail, which is a
//! much worse way to find out.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use arrow_array::{Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet};
use iris_format::{Builder, Digest, SchemaEncoding, SectionKind};
use iris_runtime::{Error, Runtime, schema_to_ipc};

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

/// The workspace root, from this crate's manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is two levels above this crate")
}

/// Compiles the fixed width decoder for wasm32, once per test binary.
fn decoder_module() -> &'static [u8] {
    static MODULE: OnceLock<Vec<u8>> = OnceLock::new();
    MODULE.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target").join("gate-wasm");

        let mut cargo = Command::new(env!("CARGO"));
        cargo
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "-p",
                "iris-decoder",
                "--example",
                "fixedwidth",
                "--target",
                "wasm32-unknown-unknown",
                "--target-dir",
            ])
            .arg(&target_dir);

        // The flags the outer build is running under are not the flags this build wants. Coverage
        // instrumentation is the one that matters: it is on for the whole workspace when the
        // coverage job runs, and it does not apply to a target with no operating system under it.
        for leaked in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "LLVM_PROFILE_FILE",
        ] {
            cargo.env_remove(leaked);
        }

        let out = cargo.output().expect("cargo is on the path, it ran this");
        assert!(
            out.status.success(),
            "building the decoder for wasm32 failed. If the target is missing, run\n  \
             rustup target add wasm32-unknown-unknown\n\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let module = target_dir
            .join("wasm32-unknown-unknown")
            .join("release")
            .join("examples")
            .join("fixedwidth.wasm");
        std::fs::read(&module)
            .unwrap_or_else(|err| panic!("cargo said it built {}: {err}", module.display()))
    })
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

#[test]
fn a_module_that_is_not_the_one_the_container_names_is_not_compiled() {
    let mut bytes = container();

    // Flip a byte somewhere in the middle, which lands in one of the two sections. Whichever one it
    // lands in, the container should notice before anything runs.
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;

    let runtime = Runtime::new().expect("the engine builds");
    match runtime.open(&bytes) {
        Err(Error::DecoderDigest { .. } | Error::Container(_)) => {}
        Err(other) => panic!("a tampered container should be refused by digest, and said: {other}"),
        Ok(_) => panic!("a tampered container opened"),
    }
}
