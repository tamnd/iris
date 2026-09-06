//! Does declaring ranges beat a well configured Parquet reader over object storage?
//!
//! This is the probe behind M5's remote scan comparison. The thesis the whole design rests on is
//! that a decoder which declares the ranges it wants lets the host fetch them well, and the place
//! that is supposed to show up is a network, where a round trip costs more than any amount of
//! arithmetic. Parquet over an object store is the thing to beat, because it is what everybody
//! already runs and because its readers have had years of work put into exactly this.
//!
//! The result stands whichever way it comes out. If declared ranges buy nothing here then the main
//! differentiator is gone, and that goes in the write up rather than into a drawer.
//!
//! Run it with
//!
//! ```text
//! cargo run --release -p iris-runtime --features probe --example remote_scan
//! ```
//!
//! against an S3 compatible endpoint named by the same five environment variables the portability
//! gate uses. Add `--json` for a machine readable object, `--rows` to change the fixture and
//! `--repeats` to change how many times each scan is run.
//!
//! # What is being compared
//!
//! One logical table, forty `i64` columns, written twice. Once as an iris container carrying the
//! fixed width decoder, and once as Parquet. Both are put in the same bucket and read back over the
//! same socket, and each is scanned two ways: every column, and three of the forty.
//!
//! Parquet is written twice rather than once, and the reason is that there is no single honest
//! setting. A deployment writes Parquet with compression and dictionaries on, so that is what
//! `parquet-tuned` is, and it moves far fewer bytes than anything uncompressed can. But a byte count
//! against a compressed file measures the codec rather than the mechanism, so `parquet-plain` is the
//! same data with compression and dictionaries off, which makes its column chunks the same size as
//! the container's columns and leaves the difference being about how each side goes and gets them.
//! Both are reported. Neither one alone is the answer.
//!
//! # Where the numbers come from
//!
//! Both sides fetch through one counting store, so requests and bytes are counted in the same place
//! by the same code. That matters more than it sounds: a comparison where each side reports its own
//! traffic through its own instrument is a comparison of two instruments. The Parquet reader here
//! implements `AsyncFileReader` over that store rather than using the one that comes with the
//! parquet crate, which is a page of code and is what makes the single instrument possible.
//!
//! Every repeat opens its dataset from scratch. A cold query pays for the container's footer or for
//! the Parquet file's footer and page index, and that is a real cost of asking a question about an
//! object nobody has looked at yet. Caching metadata across queries is a thing both sides could do
//! and neither one does here.
//!
//! Latency is wall clock around open and scan together, reported as a median over the repeats with
//! the spread next to it. The round trip time to the endpoint is measured separately and printed
//! with the results, because a comparison of request counts means nothing without it: saving a round
//! trip is worth a great deal across a region and almost nothing across a loopback interface.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use arrow_array::{Int64Array, RecordBatch};
use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::{BoxFuture, FutureExt as _};
use iris_format::Builder;
use iris_runtime::Runtime;
use iris_source::{ObjectSource, RangeSource, Readahead};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt as _, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
use parquet::arrow::async_reader::{AsyncFileReader, ParquetRecordBatchStreamBuilder};
use parquet::arrow::{ProjectionMask, arrow_writer::ArrowWriterOptions};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader};
use parquet::file::properties::{EnabledStatistics, WriterProperties};

#[path = "../tests/support/mod.rs"]
mod support;

use support::{builder, cell, schema};

/// Columns in the fixture.
///
/// Forty, because the projection claim this milestone is written against is three of forty and a
/// comparison that used a different width would not be measuring the same thing.
const COLUMNS: u64 = 40;

/// The columns a projected scan asks for.
///
/// Spread across the table rather than adjacent, so that neither side can serve them out of one
/// contiguous stretch of the object.
const PROJECTED: [usize; 3] = [7, 19, 31];

/// Rows in the fixture unless `--rows` says otherwise.
///
/// Eighty megabytes of `i64` across forty columns. Large enough that a column chunk is worth a
/// request of its own and that transfer time is not lost in the round trips, small enough that a
/// probe run finishes while somebody is still watching it.
const DEFAULT_ROWS: u64 = 250_000;

/// How many times each scan is run unless `--repeats` says otherwise.
const DEFAULT_REPEATS: usize = 5;

/// Rows per batch, on both sides.
const BATCH_ROWS: usize = 8192;

/// Rows per Parquet row group.
///
/// Chosen so the fixture holds several of them. One row group would give the Parquet reader one
/// contiguous run per column and flatter its request count against a real file, and a row group per
/// batch would give it far more metadata than a real writer produces.
const ROW_GROUP_ROWS: usize = 65_536;

/// How many times the round trip probe pings the endpoint.
const PINGS: usize = 40;

/// How far the read ahead side of the iris comparison reads ahead.
///
/// A megabyte, which is several batches of one column and a fraction of a whole column at the
/// default fixture size. Far enough that a run down one column is coalesced, not so far that the
/// first ask for a column drags the whole of it in.
const DEPTH: usize = 1024 * 1024;

/// How many runs the read ahead side keeps a block for.
///
/// One per column, because the pattern a columnar scan makes is one run per column moving forwards
/// and a single block is thrown away by every turn between them. That is the M4 result, restated
/// here as a setting.
#[expect(
    clippy::cast_possible_truncation,
    reason = "forty, on a target whose pointers are wide enough to address the fixture this probe \
              writes"
)]
const READAHEAD_STREAMS: usize = COLUMNS as usize;

// ---------------------------------------------------------------------------------------------
// The instrument
// ---------------------------------------------------------------------------------------------

/// An object store that counts what went over the socket.
///
/// Every method delegates. The only one that does anything else is `get_opts`, which is where both
/// halves of this comparison get their bytes: iris through `ObjectSource`, and the Parquet reader
/// through the `AsyncFileReader` below. Counting there rather than in each half is the point, since
/// two numbers produced by two different instruments are not a comparison.
///
/// `get_ranges` is deliberately not delegated. Leaving the trait's own implementation in place
/// means a batch of ranges arrives here as the individual requests it turns into, which is what a
/// request count is supposed to be counting.
struct Counting {
    inner: Arc<dyn ObjectStore>,
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// What has crossed the socket since the last [`Counting::reset`].
    fn taken(&self) -> Traffic {
        Traffic {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        self.requests.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

impl fmt::Debug for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Counting")
            .field("requests", &self.requests)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Counting {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        // A head request costs a round trip and brings no body back, and the store still fills in
        // the range it would have served, so counting that range would charge a length lookup for
        // the whole object. This probe was reporting a sixteen megabyte fetch on the first line of
        // every iris run until that was noticed.
        let head = options.head;
        let result = self.inner.get_opts(location, options).await?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        if !head {
            self.bytes
                .fetch_add(result.range.end - result.range.start, Ordering::Relaxed);
        }
        Ok(result)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Requests made and bytes returned.
#[derive(Clone, Copy, Debug, Default)]
struct Traffic {
    requests: u64,
    bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// The Parquet side's reader
// ---------------------------------------------------------------------------------------------

/// A Parquet reader that fetches through the counting store.
///
/// This exists instead of the parquet crate's own object store reader for two reasons, and only one
/// of them is the counting. The other is that the crate's reader is written against a different
/// major version of `object_store` than this workspace uses, and pinning two of them into the graph
/// to avoid a page of code is the worse trade.
///
/// The page index is loaded because a Parquet reader configured the way a deployment configures one
/// loads it. It costs a little more metadata on the way in and it is what lets the reader skip pages
/// rather than whole row groups, so leaving it off would be tuning the comparison in iris's favour.
struct ParquetSource {
    store: Arc<Counting>,
    path: ObjectPath,
    len: u64,
}

impl ParquetSource {
    fn new(store: Arc<Counting>, path: ObjectPath, len: u64) -> Self {
        Self { store, path, len }
    }
}

impl AsyncFileReader for ParquetSource {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        async move {
            self.store
                .get_range(&self.path, range)
                .await
                .map_err(|err| parquet::errors::ParquetError::External(Box::new(err)))
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        _options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        let len = self.len;
        async move {
            ParquetMetaDataReader::new()
                .with_page_index_policy(PageIndexPolicy::Required)
                .load_and_finish(self, len)
                .await
                .map(Arc::new)
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------------------------

/// The table both sides describe, as Arrow batches.
///
/// The same values the iris fixture holds, since `support::cell` is what fills that one. Two files
/// that do not hold the same table are not a comparison, and the check at the end of the run reads
/// three columns out of each side and compares them rather than taking that on trust.
fn batches(rows: u64) -> Vec<RecordBatch> {
    let schema = Arc::new(schema(COLUMNS));
    let mut out = Vec::new();
    let mut at = 0u64;
    while at < rows {
        let count = BATCH_ROWS.min(usize::try_from(rows - at).unwrap_or(BATCH_ROWS));
        let arrays: Vec<Arc<dyn arrow_array::Array>> = (0..COLUMNS)
            .map(|column| {
                let values: Vec<i64> = (0..count)
                    .map(|row| cell(column, at + row as u64))
                    .collect();
                Arc::new(Int64Array::from(values)) as Arc<dyn arrow_array::Array>
            })
            .collect();
        out.push(RecordBatch::try_new(Arc::clone(&schema), arrays).expect("the fixture is sound"));
        at += count as u64;
    }
    out
}

/// The two Parquet configurations, and why each one is here.
struct Settings {
    name: &'static str,
    why: &'static str,
    properties: WriterProperties,
}

fn settings() -> Vec<Settings> {
    let common = || {
        WriterProperties::builder()
            .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
            .set_write_batch_size(BATCH_ROWS)
            .set_statistics_enabled(EnabledStatistics::Page)
    };
    vec![
        Settings {
            name: "parquet-plain",
            why: "no compression and no dictionary, so its column chunks are the same size as the \
                  container's columns and the difference left is the mechanism",
            properties: common()
                .set_compression(Compression::UNCOMPRESSED)
                .set_dictionary_enabled(false)
                .set_encoding(Encoding::PLAIN)
                .build(),
        },
        Settings {
            name: "parquet-tuned",
            why: "compression and dictionaries on, which is what a deployment writes and what a \
                  fair comparison has to include even though it measures the codec as well",
            properties: common()
                .set_compression(Compression::ZSTD(
                    ZstdLevel::try_new(3).expect("three is a zstd level"),
                ))
                .set_dictionary_enabled(true)
                .build(),
        },
    ]
}

/// Writes the batches as one Parquet file.
fn parquet_bytes(batches: &[RecordBatch], properties: WriterProperties) -> Vec<u8> {
    let schema = Arc::new(schema(COLUMNS));
    let options = ArrowWriterOptions::new().with_properties(properties);
    let mut writer = ArrowWriter::try_new_with_options(Vec::new(), schema, options)
        .expect("the writer takes this schema");
    for batch in batches {
        writer.write(batch).expect("a batch this shape writes");
    }
    writer.into_inner().expect("the file closes")
}

/// The iris container, carrying the fixed width decoder that reads it.
fn container_bytes(rows: u64) -> Vec<u8> {
    let built: Builder = builder(rows, COLUMNS);
    built.build().expect("a container this size fits")
}

// ---------------------------------------------------------------------------------------------
// The measurements
// ---------------------------------------------------------------------------------------------

/// What one scan cost.
#[derive(Clone, Debug)]
struct Run {
    /// Which side, and which of its configurations.
    side: &'static str,
    /// Whether it read every column or the projection.
    shape: &'static str,
    /// Requests over the socket, median over the repeats.
    requests: u64,
    /// Bytes over the socket, median over the repeats.
    bytes: u64,
    /// Median wall clock in milliseconds, opening and scanning together.
    median_ms: f64,
    /// Fastest and slowest of the repeats, so the median is read with its spread.
    lo_ms: f64,
    hi_ms: f64,
    /// Rows the scan came back with, which is the check that it read the table.
    ///
    /// Zero on the `open` rows, which read no rows on purpose. Whatever reads this has to say which
    /// rows it means.
    rows: u64,
}

/// The shape of a run that opens a reader and stops there.
///
/// Both sides pay something before the first row: iris hashes and compiles a decoder, Parquet reads
/// a footer and a page index. Neither cost is visible in a scan number that has it folded in, and
/// the difference between them turned out to be most of the difference between the two sides, so it
/// is measured on its own rather than left for a reader to infer.
const OPEN: &str = "open";

/// The middle of a sorted set, or the lower of the two middles when there are an even number.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return f64::NAN;
    }
    values[values.len() / 2]
}

fn median_u64(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or_default()
}

/// Runs one closure `repeats` times, resetting the counter before each, and summarises it.
fn measure(
    side: &'static str,
    shape: &'static str,
    repeats: usize,
    store: &Counting,
    mut body: impl FnMut() -> u64,
) -> Run {
    let mut millis = Vec::with_capacity(repeats);
    let mut requests = Vec::with_capacity(repeats);
    let mut bytes = Vec::with_capacity(repeats);
    let mut rows = 0;

    for _ in 0..repeats {
        store.reset();
        let started = Instant::now();
        rows = body();
        millis.push(started.elapsed().as_secs_f64() * 1e3);
        let taken = store.taken();
        requests.push(taken.requests);
        bytes.push(taken.bytes);
    }

    let lo = millis.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = millis.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Run {
        side,
        shape,
        requests: median_u64(&mut requests),
        bytes: median_u64(&mut bytes),
        median_ms: median(&mut millis),
        lo_ms: lo,
        hi_ms: hi,
        rows,
    }
}

/// Median round trip time to the endpoint, in microseconds.
///
/// One byte fetched from an object that is already there, repeated. It is not a measurement of the
/// endpoint's throughput and is not meant to be. It is the number that says how much a saved
/// request is worth, without which a comparison of request counts cannot be read at all.
fn round_trip(tokio: &tokio::runtime::Runtime, store: &Counting, path: &ObjectPath) -> f64 {
    let mut samples = Vec::with_capacity(PINGS);
    for _ in 0..PINGS {
        let started = Instant::now();
        tokio
            .block_on(store.get_range(path, 0..1))
            .expect("the endpoint serves one byte");
        samples.push(started.elapsed().as_secs_f64() * 1e6);
    }
    median(&mut samples)
}

// ---------------------------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------------------------

/// The endpoint, from the same five variables the portability gate reads.
struct Endpoint {
    url: String,
    bucket: String,
    key_id: String,
    secret: String,
    region: String,
}

impl Endpoint {
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

    fn store(&self) -> Arc<dyn ObjectStore> {
        Arc::new(
            AmazonS3Builder::new()
                .with_endpoint(&self.url)
                .with_bucket_name(&self.bucket)
                .with_access_key_id(&self.key_id)
                .with_secret_access_key(&self.secret)
                .with_region(&self.region)
                .with_virtual_hosted_style_request(false)
                .with_allow_http(true)
                .build()
                .expect("the endpoint configuration builds a store"),
        )
    }
}

/// What the command line asked for.
struct Options {
    rows: u64,
    repeats: usize,
    json: bool,
}

fn options() -> Options {
    let mut out = Options {
        rows: DEFAULT_ROWS,
        repeats: DEFAULT_REPEATS,
        json: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => out.json = true,
            "--rows" => {
                out.rows = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--rows takes a number");
            }
            "--repeats" => {
                out.repeats = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--repeats takes a number");
            }
            other => panic!("unknown argument {other}, this takes --json, --rows and --repeats"),
        }
    }
    out
}

fn main() {
    let options = options();
    let endpoint = match Endpoint::from_env() {
        Ok(endpoint) => endpoint,
        Err(why) => {
            eprintln!(
                "this probe needs an S3 compatible endpoint to run against: {why}.\n\
                 It reads the same five variables the portability gate does: AWS_ENDPOINT_URL, \
                 IRIS_TEST_BUCKET, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and AWS_REGION."
            );
            std::process::exit(2);
        }
    };

    let tokio = tokio::runtime::Runtime::new().expect("a tokio runtime starts");
    let store = Arc::new(Counting::new(endpoint.store()));
    let _guard = tokio.enter();

    // Everything is built before anything is uploaded, so a fixture that does not write is a
    // failure before the endpoint has been touched.
    let table = batches(options.rows);
    let container = container_bytes(options.rows);
    let files: Vec<(Settings, Vec<u8>)> = settings()
        .into_iter()
        .map(|setting| {
            let bytes = parquet_bytes(&table, setting.properties.clone());
            (setting, bytes)
        })
        .collect();

    let container_path = ObjectPath::from("remote-scan/iris.iris");
    let container_len = container.len() as u64;
    tokio
        .block_on(store.put(&container_path, container.into()))
        .expect("the container uploads");

    let mut parquet_paths = Vec::new();
    for (setting, bytes) in &files {
        let path = ObjectPath::from(format!("remote-scan/{}.parquet", setting.name));
        let len = bytes.len() as u64;
        tokio
            .block_on(store.put(&path, bytes.clone().into()))
            .expect("the Parquet fixture uploads");
        parquet_paths.push((setting.name, setting.why, path, len));
    }

    let rtt_us = round_trip(&tokio, &store, &container_path);
    let runs = scans(
        &tokio,
        &store,
        options.repeats,
        &container_path,
        &parquet_paths,
    );

    // Both sides have to be describing the same table, or none of the above is a comparison. Read
    // the projection out of each and compare it against what the fixture says it should be.
    let checked = agree(&files, &table);

    if options.json {
        json(&options, &endpoint, &runs, rtt_us, container_len, &files);
    } else {
        human(
            &options,
            &endpoint,
            &runs,
            rtt_us,
            container_len,
            &parquet_paths,
            checked,
        );
    }

    for (_, _, path, _) in &parquet_paths {
        tokio
            .block_on(store.delete(path))
            .expect("the fixture is removed");
    }
    tokio
        .block_on(store.delete(&container_path))
        .expect("the fixture is removed");
}

/// Every run this probe measures, in the order they are reported.
///
/// Each measured closure opens its own reader, so a repeat pays for the footer the way a cold query
/// does. The counting store is reset around each repeat by [`measure`] rather than here, so the
/// uploads and the round trip probe above are outside every number below.
fn scans(
    tokio: &tokio::runtime::Runtime,
    store: &Arc<Counting>,
    repeats: usize,
    container_path: &ObjectPath,
    parquet_paths: &[(&'static str, &'static str, ObjectPath, u64)],
) -> Vec<Run> {
    let mut runs = iris_runs(tokio, store, repeats, container_path);
    runs.extend(parquet_runs(tokio, store, repeats, parquet_paths));
    runs
}

/// The iris side of the comparison.
fn iris_runs(
    tokio: &tokio::runtime::Runtime,
    store: &Arc<Counting>,
    repeats: usize,
    container_path: &ObjectPath,
) -> Vec<Run> {
    let runtime = Runtime::new()
        .expect("the engine builds")
        .with_max_batch_rows(BATCH_ROWS as u64);

    let projection: Vec<u32> = PROJECTED
        .iter()
        .map(|&c| u32::try_from(c).expect("a column index fits"))
        .collect();

    let mut runs = Vec::new();

    // The iris side. A fresh source and a fresh dataset per repeat, because the footer read is part
    // of what a cold query pays for and caching it here would be measuring the second query.
    //
    // Twice, because a bare source is not what a host would deploy. The decoder asks for a batch of
    // one column at a time and each of those is a round trip on its own, which is the pattern
    // `Readahead` was built in M4 to coalesce, so leaving it off would be comparing a tuned Parquet
    // reader against an untuned iris. Both are reported, since the difference between them is the
    // clearest statement of what host side coalescing is worth over a network.
    let iris_open = |ahead: bool| {
        let source = tokio
            .block_on(ObjectSource::open(
                Arc::clone(store) as Arc<dyn ObjectStore>,
                container_path.clone(),
            ))
            .expect("the object opens");
        let source: Box<dyn RangeSource + Send> = if ahead {
            Box::new(Readahead::new(source, DEPTH).with_streams(READAHEAD_STREAMS))
        } else {
            Box::new(source)
        };
        runtime
            .open_windowed(source)
            .expect("the container opens over the network")
    };

    let iris_scan = |columns: &[u32], ahead: bool| {
        let mut dataset = iris_open(ahead);
        let batches = if columns.is_empty() {
            dataset.scan()
        } else {
            dataset.scan_columns(columns)
        }
        .expect("the decoder runs over the network");
        batches.iter().map(|b| b.num_rows() as u64).sum::<u64>()
    };

    // Opening on its own first, so the scan rows below can be read as a scan plus this rather than
    // as one number with a decoder compile hidden inside it. Without readahead, because opening
    // reads the trailer, the header, the footer and the decoder section and nothing else, and a
    // block big enough to coalesce a scan is bigger than any of them.
    runs.push(measure("iris", OPEN, repeats, store, || {
        drop(iris_open(false));
        0
    }));
    runs.push(measure("iris", "all 40", repeats, store, || {
        iris_scan(&[], false)
    }));
    runs.push(measure("iris", "3 of 40", repeats, store, || {
        iris_scan(&projection, false)
    }));
    runs.push(measure("iris-ahead", "all 40", repeats, store, || {
        iris_scan(&[], true)
    }));
    runs.push(measure("iris-ahead", "3 of 40", repeats, store, || {
        iris_scan(&projection, true)
    }));

    runs
}

/// The Parquet side of the comparison, the same shapes against each configuration.
fn parquet_runs(
    tokio: &tokio::runtime::Runtime,
    store: &Arc<Counting>,
    repeats: usize,
    parquet_paths: &[(&'static str, &'static str, ObjectPath, u64)],
) -> Vec<Run> {
    let mut runs = Vec::new();
    for (name, _why, path, len) in parquet_paths {
        let open = || {
            let source = ParquetSource::new(Arc::clone(store), path.clone(), *len);
            tokio
                .block_on(ParquetRecordBatchStreamBuilder::new_with_options(
                    source,
                    ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required),
                ))
                .expect("the Parquet footer reads")
        };

        let scan = |projected: bool| {
            let builder = open();
            tokio.block_on(async {
                let builder = if projected {
                    let mask = ProjectionMask::roots(builder.parquet_schema(), PROJECTED);
                    builder.with_projection(mask)
                } else {
                    builder
                };
                let mut stream = builder
                    .with_batch_size(BATCH_ROWS)
                    .build()
                    .expect("the reader builds");
                let mut rows = 0u64;
                while let Some(batch) = stream.next().await {
                    rows += batch.expect("a batch reads").num_rows() as u64;
                }
                rows
            })
        };

        // The same split as the iris side. For Parquet this is the footer and the page index, which
        // is what a reader has to have in hand before it can decide which pages to ask for.
        runs.push(measure(name, OPEN, repeats, store, || {
            drop(open());
            0
        }));
        runs.push(measure(name, "all 40", repeats, store, || scan(false)));
        runs.push(measure(name, "3 of 40", repeats, store, || scan(true)));
    }

    runs
}

/// Checks the Parquet files hold the table the iris container holds.
///
/// Read locally out of the bytes that were uploaded, rather than over the network, because this is
/// about what is in the files and not about how they are fetched. It returns how many values it
/// compared so that the report can say so rather than saying it checked.
fn agree(files: &[(Settings, Vec<u8>)], table: &[RecordBatch]) -> u64 {
    let expected: Vec<Vec<i64>> = PROJECTED
        .iter()
        .map(|&column| {
            table
                .iter()
                .flat_map(|batch| {
                    batch
                        .column(column)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .expect("an Int64 field")
                        .values()
                        .to_vec()
                })
                .collect()
        })
        .collect();

    let mut compared = 0u64;
    for (setting, bytes) in files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
            .expect("the fixture is a Parquet file");
        let mask = ProjectionMask::roots(reader.parquet_schema(), PROJECTED);
        let reader = reader
            .with_projection(mask)
            .with_batch_size(BATCH_ROWS)
            .build()
            .expect("the reader builds");

        let mut got: Vec<Vec<i64>> = vec![Vec::new(); PROJECTED.len()];
        for batch in reader {
            let batch = batch.expect("a batch reads");
            for (at, values) in got.iter_mut().enumerate() {
                values.extend_from_slice(
                    batch
                        .column(at)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .expect("an Int64 field")
                        .values(),
                );
            }
        }

        assert_eq!(
            got, expected,
            "{} does not hold the table the container holds, so there is nothing to compare",
            setting.name
        );
        compared += got.iter().map(|v| v.len() as u64).sum::<u64>();
    }
    compared
}

#[expect(
    clippy::cast_precision_loss,
    reason = "byte counts and row counts from one fixture, turned into ratios for a report"
)]
fn human(
    options: &Options,
    endpoint: &Endpoint,
    runs: &[Run],
    rtt_us: f64,
    container_len: u64,
    files: &[(&'static str, &'static str, ObjectPath, u64)],
    checked: u64,
) {
    println!("iris against Parquet over object storage");
    println!();
    println!(
        "Fixture: {} rows of {COLUMNS} int64 columns, scanned whole and as {} of {COLUMNS}.",
        options.rows,
        PROJECTED.len()
    );
    println!("Endpoint: {}, bucket {}.", endpoint.url, endpoint.bucket);
    println!(
        "Round trip to it: {rtt_us:.0} microseconds, median of {PINGS} one byte fetches. A saved \
         request is worth about that much."
    );
    println!(
        "Repeats: {}, each one opening its dataset cold.",
        options.repeats
    );
    println!();
    println!("Object sizes:");
    println!("  iris container  {container_len:>12} bytes");
    for (name, _, _, len) in files {
        println!("  {name:<14}  {len:>12} bytes");
    }
    println!();
    println!(
        "{:<14} {:<8} {:>9} {:>14} {:>11} {:>18}",
        "side", "shape", "requests", "bytes", "median ms", "spread ms"
    );
    for run in runs {
        println!(
            "{:<14} {:<8} {:>9} {:>14} {:>11.1} {:>8.1} to {:.1}",
            run.side, run.shape, run.requests, run.bytes, run.median_ms, run.lo_ms, run.hi_ms
        );
    }
    println!();

    let mut scanned = runs.iter().filter(|r| r.shape != OPEN).map(|r| r.rows);
    println!(
        "Every scan came back with the rows it asked for: {}.",
        if scanned.all(|r| r == options.rows) {
            "yes"
        } else {
            "no, which invalidates the run above"
        }
    );
    println!(
        "The Parquet files hold the table the container holds: checked over {checked} values."
    );
    println!();

    for (name, why, _, _) in files {
        println!("  {name}: {why}");
    }
    println!();

    // What each side pays before it has read a row. Stated on its own because it is a fixed cost
    // that does not move with the fixture, so on a bigger scan it matters less and on a smaller one
    // it is the whole difference, and a reader who only has the scan rows cannot tell which.
    let open_ms = |side: &str| {
        runs.iter()
            .find(|r| r.side == side && r.shape == OPEN)
            .map_or(f64::NAN, |r| r.median_ms)
    };
    println!(
        "Before the first row: iris spends {:.1} ms opening the container, which is the trailer, \
         the footer, the decoder section, the hash of that section and compiling it. The Parquet \
         reader spends {:.1} ms on the footer and the page index. That cost is inside every scan \
         row in the table above, and it is the same for a scan of one column as for a scan of all \
         of them.",
        open_ms("iris"),
        open_ms(files.first().map_or("", |(name, _, _, _)| name)),
    );
    println!();

    // The two comparisons worth stating outright, rather than leaving a reader to divide the table.
    for shape in ["all 40", "3 of 40"] {
        let iris = runs
            .iter()
            .find(|r| r.side == "iris-ahead" && r.shape == shape)
            .expect("the iris rows are there");
        for run in runs
            .iter()
            .filter(|r| !r.side.starts_with("iris") && r.shape == shape)
        {
            println!(
                "{shape}: iris made {} requests moving {} bytes in {:.1} ms, {} made {} requests \
                 moving {} bytes in {:.1} ms. That is {:.2} times the requests, {:.2} times the \
                 bytes and {:.2} times the time.",
                iris.requests,
                iris.bytes,
                iris.median_ms,
                run.side,
                run.requests,
                run.bytes,
                run.median_ms,
                run.requests as f64 / iris.requests.max(1) as f64,
                run.bytes as f64 / iris.bytes.max(1) as f64,
                run.median_ms / iris.median_ms,
            );
        }
    }
}

fn json(
    options: &Options,
    endpoint: &Endpoint,
    runs: &[Run],
    rtt_us: f64,
    container_len: u64,
    files: &[(Settings, Vec<u8>)],
) {
    let sizes: Vec<String> = files
        .iter()
        .map(|(setting, bytes)| format!("\"{}\":{}", setting.name, bytes.len()))
        .collect();
    let rows: Vec<String> = runs
        .iter()
        .map(|run| {
            format!(
                "{{\"side\":\"{}\",\"shape\":\"{}\",\"requests\":{},\"bytes\":{},\
                 \"median_ms\":{:.3},\"lo_ms\":{:.3},\"hi_ms\":{:.3},\"rows\":{}}}",
                run.side,
                run.shape,
                run.requests,
                run.bytes,
                run.median_ms,
                run.lo_ms,
                run.hi_ms,
                run.rows
            )
        })
        .collect();

    println!(
        "{{\"probe\":\"remote_scan\",\"target\":\"{}\",\"rows\":{},\"columns\":{COLUMNS},\
         \"projected\":{:?},\"repeats\":{},\"endpoint\":\"{}\",\"bucket\":\"{}\",\
         \"round_trip_us\":{rtt_us:.1},\"sizes\":{{\"iris\":{container_len},{}}},\"runs\":[{}]}}",
        target(),
        options.rows,
        PROJECTED,
        options.repeats,
        endpoint.url,
        endpoint.bucket,
        sizes.join(","),
        rows.join(",")
    );
}

/// The target triple this was built for, which is the only hardware fact the binary itself knows.
fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}
