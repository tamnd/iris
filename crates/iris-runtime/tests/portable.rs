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
//!
//! # The second gate
//!
//! The other test here is the reconciliation half of #27. A scan reports how many requests it made
//! and how many bytes came back, and a number a program computes about itself is worth very little
//! on its own. So it is checked against what the server on the other end of the socket wrote in its
//! own counters, which is the one observer that has no reason to agree.
//!
//! That half is not portable and does not pretend to be: it reads a Prometheus text endpoint and
//! knows the names of an S3 server's metrics. It is pointed at one by an environment variable and
//! it says so when it is not.
//!
//! # The third gate
//!
//! The last test here is the measurement half of #28. Reading ahead of a scan is only worth having
//! if it makes a scan cheaper, and whether it does depends on the access pattern rather than on the
//! idea, so it is measured against the pattern this fixture actually produces rather than against a
//! run of adjacent requests written to make it look good. The same scan is run twice over the same
//! object, once with the source as it is and once with the same source read ahead of, and the two
//! are compared on request count and on the answer they came back with.

// Out of the build under loom, where the object storage dev-dependencies are out of the graph
// because tokio compiled with that flag has no `tokio::net` for hyper to reach for. This file is
// the only thing in the crate that wants them.
#![cfg(not(loom))]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;

use arrow_array::RecordBatch;
use iris_format::Digest;
use iris_runtime::Runtime;
use iris_source::{FileSource, ObjectSource, RangeSource, Readahead};
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

/// How far the readahead gate reads ahead.
///
/// Several times a batch of one column and a fraction of a whole column, so that a run of requests
/// down one column is coalesced and the whole column is still not dragged in by the first ask for it.
const DEPTH: usize = 128 * 1024;

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

#[test]
#[ignore = "needs an S3 compatible endpoint, run by name from the object storage job"]
fn reading_ahead_of_a_scan_costs_fewer_requests_for_the_same_answer() {
    let endpoint = match Endpoint::from_env() {
        Ok(endpoint) => endpoint,
        Err(why) => panic!(
            "this gate was run without an endpoint to run against: {why}. \
             The object storage job in ci.yml starts one and exports the five variables."
        ),
    };

    let builder = builder(ROWS, COLUMNS);
    let bytes = builder.build().expect("a container this small always fits");

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);

    let tokio = tokio::runtime::Runtime::new().expect("a tokio runtime starts");
    let store = endpoint.store();
    let key = ObjectPath::from("readahead.iris");
    tokio
        .block_on(store.put(&key, bytes.into()))
        .expect("the fixture uploads");

    let _guard = tokio.enter();

    // The same scan twice over the same object. The only difference between the two is what the host
    // put in front of the source, which is the claim: a decoder cannot see this and does not change.
    let plain = {
        let object = tokio
            .block_on(ObjectSource::open(Arc::clone(&store), key.clone()))
            .expect("the object opens");
        let mut dataset = runtime
            .open_windowed(Box::new(object))
            .expect("the container opens over the network");
        let batches = dataset.scan().expect("the decoder runs over the network");
        (columns(&batches, COLUMNS), dataset.last_scan())
    };

    let ahead = {
        let object = tokio
            .block_on(ObjectSource::open(Arc::clone(&store), key.clone()))
            .expect("the object opens");
        // One stream per column, because the pattern a columnar scan makes is one run per column
        // moving forwards, and a single block is thrown away by every turn between them.
        let source = Readahead::new(object, DEPTH)
            .with_streams(usize::try_from(COLUMNS).expect("three columns fit in a usize"));
        let mut dataset = runtime
            .open_windowed(Box::new(source))
            .expect("the container opens over the network");
        let batches = dataset.scan().expect("the decoder runs over the network");
        (columns(&batches, COLUMNS), dataset.last_scan())
    };

    // First, that it is the same scan. Coalescing that changes the answer is not coalescing, and a
    // request count is only worth comparing between two runs that agree about what they read.
    assert_eq!(
        ahead.0, plain.0,
        "the scan came back differently when the host read ahead of it"
    );

    let (plain, ahead) = (plain.1, ahead.1);
    assert!(
        plain.requests > 1,
        "a scan that made {} requests without readahead has nothing to coalesce",
        plain.requests
    );
    assert!(
        ahead.requests * 2 <= plain.requests,
        "reading ahead took the scan from {} requests to {}, which is not a reduction worth the \
         memory it costs",
        plain.requests,
        ahead.requests
    );

    // And it did not buy that by moving the data twice. A full scan reads the whole data section
    // whichever way it is served, so the two byte counts are the same number plus whatever the last
    // block of each run overshot by.
    assert!(
        ahead.bytes <= plain.bytes * 2,
        "reading ahead moved {} bytes where reading exactly what was asked for moved {}",
        ahead.bytes,
        plain.bytes
    );

    tokio
        .block_on(store.delete(&key))
        .expect("the fixture is removed from the store");
}

/// What the endpoint says it has served, read from its own counters rather than from ours.
///
/// Two numbers out of the S3 server's Prometheus text endpoint. `GetObject` is the call a range
/// request arrives as, and the bytes are everything the server sent over the S3 API, which is the
/// response bodies plus their headers plus whatever else was asked of it in the meantime. So the
/// request count reconciles exactly and the byte count reconciles as a bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Recorded {
    gets: u64,
    sent: u64,
}

impl Recorded {
    /// Scrapes the endpoint, waiting until two readings in a row agree.
    ///
    /// The settling is not the test looking for the answer it wants. It is a fixed rule applied
    /// before anything is compared, because a counter that is still being written when it is read
    /// gives a number that belongs to no moment at all. Whatever value it settles on is the value
    /// the assertions run against, right or wrong.
    fn scrape(url: &str) -> Self {
        let mut last = Self::read(url);
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let now = Self::read(url);
            if now == last {
                return now;
            }
            last = now;
        }
        panic!("the endpoint's counters never stopped moving, last reading {last:?}");
    }

    /// One reading.
    ///
    /// A hand written request rather than an HTTP client, which is worth a sentence because it is
    /// normally the wrong instinct. This is one GET to a loopback address with no authentication,
    /// asking for HTTP/1.0, so the server answers, closes, and the body is everything up to the
    /// end of the stream. There is no chunked encoding to decode, no redirect to follow and no
    /// connection to reuse. Pulling an HTTP stack into this crate's dev-dependencies to avoid
    /// fifteen lines would be the larger change.
    fn read(url: &str) -> Self {
        let rest = url
            .strip_prefix("http://")
            .expect("the metrics URL is plain HTTP to a server on this machine");
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };

        let mut socket = TcpStream::connect(authority)
            .unwrap_or_else(|err| panic!("connecting to {authority} for metrics: {err}"));
        socket
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\n\r\n").as_bytes())
            .expect("asking for the metrics");

        let mut body = String::new();
        socket
            .read_to_string(&mut body)
            .expect("reading the metrics back");

        Self {
            gets: sample(
                &body,
                "minio_api_requests_total{",
                Some("name=\"GetObject\""),
            ),
            sent: sample(&body, "minio_api_requests_traffic_sent_bytes{", None),
        }
    }
}

/// The value of the first metric line that starts with `metric` and mentions `label`.
///
/// Zero when there is no such line, which is what a counter that has never been incremented looks
/// like on this endpoint: it is simply absent until the first time it moves.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a request count and a byte count from a server that started a minute ago, read \
              through a float because that is what the exposition format says they are"
)]
fn sample(body: &str, metric: &str, label: Option<&str>) -> u64 {
    for line in body.lines() {
        if !line.starts_with(metric) {
            continue;
        }
        if label.is_some_and(|label| !line.contains(label)) {
            continue;
        }
        let value = line
            .rsplit_once(' ')
            .expect("a Prometheus sample is a series and a value")
            .1;
        // Parsed as a float because that is what the format says it is, and this endpoint really
        // does write six figure byte counts in scientific notation.
        let value: f64 = value
            .parse()
            .unwrap_or_else(|err| panic!("the value on {line:?} is not a number: {err}"));
        return value as u64;
    }
    0
}

#[test]
#[ignore = "needs an S3 compatible endpoint, run by name from the object storage job"]
fn what_a_scan_says_it_moved_is_what_the_endpoint_recorded() {
    let endpoint = match Endpoint::from_env() {
        Ok(endpoint) => endpoint,
        Err(why) => panic!(
            "this gate was run without an endpoint to run against: {why}. \
             The object storage job in ci.yml starts one and exports the five variables."
        ),
    };
    let metrics = std::env::var("IRIS_TEST_METRICS_URL").unwrap_or_else(|_| {
        panic!(
            "IRIS_TEST_METRICS_URL is not set, so there is nothing to reconcile against. \
             It is the S3 server's Prometheus endpoint, and the server has to have been started \
             with its metrics readable without a token."
        )
    });

    let builder = builder(ROWS, COLUMNS);
    let bytes = builder.build().expect("a container this small always fits");
    let len = bytes.len() as u64;

    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS);

    let tokio = tokio::runtime::Runtime::new().expect("a tokio runtime starts");
    let store = endpoint.store();
    let key = ObjectPath::from("reconcile.iris");
    tokio
        .block_on(store.put(&key, bytes.into()))
        .expect("the fixture uploads");

    let object = tokio
        .block_on(ObjectSource::open(Arc::clone(&store), key.clone()))
        .expect("the object opens");
    assert_eq!(object.len(), len);

    let _guard = tokio.enter();
    let mut dataset = runtime
        .open_windowed(Box::new(object))
        .expect("the container opens over the network");

    // Read after opening, so the trailer, the header, the footer and the decoder module are on the
    // far side of the line. What is being reconciled is a scan.
    let before = Recorded::scrape(&metrics);

    let batches = dataset.scan().expect("the decoder runs over the network");
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows as u64, ROWS, "the scan did not read the dataset");

    let scan = dataset.last_scan();
    let after = Recorded::scrape(&metrics);
    let gets = after.gets - before.gets;
    let sent = after.sent - before.sent;

    assert!(
        scan.requests > 1,
        "a scan of this container over a network made {} requests, which is too few to be \
         reconciling anything interesting",
        scan.requests
    );
    assert_eq!(
        scan.requests, gets,
        "the scan says it made {} requests and the endpoint recorded {gets}",
        scan.requests
    );

    // Bytes reconcile as a bound rather than exactly, and the reason is on the server's side of the
    // wire: its counter is everything it sent over the S3 API, which includes a response header per
    // request. So the scan's number has to be no larger than the endpoint's, and the difference has
    // to be small enough to be headers rather than a second copy of the data.
    assert!(
        sent >= scan.bytes,
        "the scan says {} bytes came back and the endpoint only sent {sent}",
        scan.bytes
    );
    let overhead = sent - scan.bytes;
    assert!(
        overhead <= gets * 4_096,
        "{overhead} bytes of the {sent} the endpoint sent are not accounted for by the {} the \
         scan counted, across {gets} requests",
        scan.bytes
    );

    tokio
        .block_on(store.delete(&key))
        .expect("the fixture is removed from the store");
}
