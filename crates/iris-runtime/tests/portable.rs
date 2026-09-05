//! The M4 portability gate: one decoder, two places the bytes live, one answer.
//!
//! The claim the whole design rests on is that a decoder does not know where its bytes come from. It
//! names ranges and the host produces them, so the same module reads a local file and an object in a
//! bucket without being rebuilt, reconfigured or given a different entry point. If any of those were
//! needed, the abstraction would have leaked and the claim would not be true.
//!
//! So the gate is deliberately narrow. One container is built once, carrying one decoder module. The
//! same bytes are written to a file and put in a bucket. Both are opened the same way, through
//! `Runtime::open_windowed`, and the test asserts that the decoder that ran was the same one, by
//! digest, and that what came back was identical row for row. The local file is removed before the
//! object is read, so the second half cannot quietly be reading the first half's bytes.
//!
//! # Why it is ignored
//!
//! It needs an S3 compatible endpoint, and a test that silently passes when there is no endpoint is
//! worse than one that does not run: a gate nobody notices has stopped running is a gate that is not
//! there. So this is ignored in the tree, the CI job that starts an S3 server runs it by name, and
//! the configuration comes in through the environment rather than through a default that would
//! point at somebody's real bucket if it were ever wrong.
//!
//! The decoder is compiled rather than checked in. See `tests/support/mod.rs`.

use std::sync::Arc;

use arrow_array::RecordBatch;
use iris_format::Digest;
use iris_runtime::Runtime;
use iris_source::{FileSource, ObjectSource, RangeSource};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt as _};

mod support;

use support::{builder, cell, column_values, decoder_module, write_container};

/// Rows in the fixture. Enough that a scan comes back in several batches and the object source has
/// to fetch more than once, and small enough that a CI job spends no real time on it.
const ROWS: u64 = 50_000;

/// Columns in the fixture. More than one, so a column offset that is wrong by a header is wrong by a
/// whole column here rather than by nothing.
const COLUMNS: u64 = 3;

/// Small enough that a scan comes back in several batches, which is when a source has to move.
const BATCH_ROWS: u64 = 4_096;

/// Where this gate keeps its fixture, which the CI job removes after a run that failed.
const SCRATCH: &str = "gate-object";

/// What this run was told about the store, or nothing if it was told nothing.
///
/// Read from the environment rather than defaulted, because a default endpoint is a default that
/// eventually points somewhere real. The names are the ones the AWS tools already use, so a
/// developer with an S3 server running locally can export what they already have.
struct Endpoint {
    url: String,
    bucket: String,
    key_id: String,
    secret: String,
    region: String,
}

impl Endpoint {
    /// The configuration, or a message saying which part of it is missing.
    fn from_env() -> Result<Self, String> {
        let var = |name: &str| {
            std::env::var(name).map_err(|_| format!("{name} is not set, so there is no endpoint"))
        };
        Ok(Self {
            url: var("AWS_ENDPOINT_URL")?,
            bucket: var("IRIS_TEST_BUCKET")?,
            key_id: var("AWS_ACCESS_KEY_ID")?,
            secret: var("AWS_SECRET_ACCESS_KEY")?,
            region: std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_owned()),
        })
    }

    /// A store pointed at the bucket.
    ///
    /// Path style addressing and plain HTTP, because that is what the server on a CI runner speaks.
    /// A host talking to a real bucket would not set either, and neither choice reaches the decoder,
    /// which is the point being made.
    fn store(&self) -> Arc<dyn ObjectStore> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&self.url)
            .with_bucket_name(&self.bucket)
            .with_access_key_id(&self.key_id)
            .with_secret_access_key(&self.secret)
            .with_region(&self.region)
            .with_virtual_hosted_style_request(false)
            .with_allow_http(true)
            .build()
            .expect("the endpoint configuration builds a store");
        Arc::new(store)
    }
}

/// Every column of `batches`, so that two runs can be compared in one line.
fn columns(batches: &[RecordBatch], count: u64) -> Vec<Vec<i64>> {
    (0..count)
        .map(|c| {
            column_values(
                batches,
                usize::try_from(c).expect("three columns fit in a usize"),
            )
        })
        .collect()
}

#[test]
#[ignore = "needs an S3 compatible endpoint, run by name from the object storage job"]
fn one_decoder_reads_a_file_and_an_object_and_says_the_same_thing() {
    let endpoint = match Endpoint::from_env() {
        Ok(endpoint) => endpoint,
        Err(why) => panic!(
            "this gate was run without an endpoint to run against: {why}. \
             The object storage job in ci.yml starts one and exports the five variables."
        ),
    };

    // One container, built once. Everything after this point reads these bytes, so "the same
    // artifact" is a fact about the fixture rather than something the test has to check twice.
    let builder = builder(ROWS, COLUMNS);
    let bytes = builder.build().expect("a container this small always fits");
    let expected = Digest::of(decoder_module());

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);

    // The file half.
    let (scratch, len) = write_container(SCRATCH, "portable", &builder);
    assert_eq!(len, bytes.len() as u64, "the two writers disagree");

    let (file_digest, file_name, file_schema, file_rows, file_columns, file_range) = {
        let source = FileSource::open(&scratch.0).expect("the fixture opens");
        let mut dataset = runtime
            .open_windowed(Box::new(source))
            .expect("the container opens through a window");
        let all = dataset.scan().expect("the decoder runs");
        let range = dataset.scan_rows(1_000, 500).expect("the decoder runs");
        (
            dataset.decoder_digest(),
            dataset.name().to_owned(),
            dataset.schema().clone(),
            dataset.rows(),
            columns(&all, COLUMNS),
            columns(&range, COLUMNS),
        )
    };

    // The object half. The runtime is a real one because the object source spawns its fetches onto
    // whatever the host already has, which is the arrangement a host actually uses.
    let tokio = tokio::runtime::Runtime::new().expect("a tokio runtime starts");
    let store = endpoint.store();
    let key = ObjectPath::from("portable.iris");
    tokio
        .block_on(store.put(&key, bytes.clone().into()))
        .expect("the fixture uploads");

    // The file goes now, before anything reads the object. Whatever the second half comes back with,
    // it did not come from here.
    drop(scratch);

    let object = tokio
        .block_on(ObjectSource::open(Arc::clone(&store), key.clone()))
        .expect("the object opens");
    assert_eq!(
        object.len(),
        len,
        "the store is holding something other than what was uploaded"
    );

    let _guard = tokio.enter();
    let mut dataset = runtime
        .open_windowed(Box::new(object))
        .expect("the container opens over the network");

    // The identity check, which is the gate. Not "a decoder called fixedwidth ran twice" but "these
    // are the same bytes", and the same bytes as the artifact the build produced.
    assert_eq!(
        dataset.decoder_digest(),
        file_digest,
        "the two paths ran decoders with different digests"
    );
    assert_eq!(
        dataset.decoder_digest(),
        expected,
        "neither path ran the module this test built"
    );

    assert_eq!(
        dataset.name(),
        file_name,
        "the name changed with the source"
    );
    assert_eq!(
        dataset.schema(),
        &file_schema,
        "the schema changed with the source"
    );
    assert_eq!(
        dataset.rows(),
        file_rows,
        "the row count changed with the source"
    );

    let all = dataset.scan().expect("the decoder runs over the network");
    let range = dataset
        .scan_rows(1_000, 500)
        .expect("the decoder runs over the network");

    assert_eq!(
        columns(&all, COLUMNS),
        file_columns,
        "a full scan came back differently over the network"
    );
    assert_eq!(
        columns(&range, COLUMNS),
        file_range,
        "a row range came back differently over the network"
    );

    // And both agree with the fixture, so that two paths agreeing on the wrong answer is not a pass.
    for (index, values) in file_columns.iter().enumerate() {
        let column = u64::try_from(index).expect("three columns fit in a u64");
        assert_eq!(values.len() as u64, ROWS, "column {column} came back short");
        for (row, value) in values.iter().enumerate() {
            let row = u64::try_from(row).expect("fifty thousand rows fit in a u64");
            assert_eq!(*value, cell(column, row), "column {column} row {row}");
        }
    }

    tokio
        .block_on(store.delete(&key))
        .expect("the fixture is removed from the store");
}
