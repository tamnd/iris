//! The decoders the gate tests drive, compiled for wasm32 on the way in.
//!
//! # Why it builds the decoder rather than checking one in
//!
//! A committed `.wasm` fixture is a binary nobody reads, built by a toolchain nobody remembers,
//! that keeps passing after the source it came from has stopped matching it. The interesting
//! failure here is the ABI drifting away from the SDK, and a stale fixture is precisely the thing
//! that hides it. So the decoder is compiled from `crates/iris-decoder/examples/fixedwidth.rs`
//! every time these tests run.
//!
//! The cost is that running the test suite needs the wasm32 target installed. `rust-toolchain.toml`
//! asks for it, so rustup puts it there without anybody thinking about it, and the failure message
//! says what to do if somebody is running a toolchain that ignored the file.
//!
//! The nested cargo gets its own target directory. Cargo's lock is per target directory, so
//! building into the one the outer cargo is holding would deadlock rather than fail, which is a
//! much worse way to find out.
//!
//! It also gets a lock of its own, and that one is not obvious. nextest runs every test in its own
//! process, so the tests across these files are that many cargo invocations against one target
//! directory. Cargo's lock makes them build one at a time, which is all it promises. It does not
//! cover the reads afterwards, and cargo finishes a build by moving the example from `deps` to
//! `examples`, which it does by removing the destination and linking it again. It does that every
//! time, even when nothing needed compiling. So a process that has just released the lock and is
//! reading the files can be reading them while the next process removes one, and the read fails with
//! a missing file after cargo said it built it. The lock here is held across the build and the reads
//! together, which is the only arrangement where what was built is still there to be read.
//!
//! Both examples are built by one cargo invocation rather than one each. Almost all of the time
//! goes on the SDK and its dependencies, which is paid once either way, so the second decoder is
//! close to free and two nested builds would not be.
//!
//! Every test file here gets its own copy of this module, and no one file uses all of it, which is
//! what the allow below is for. Splitting it up so that each file sees only what it calls would mean
//! two ways to run a nested cargo, which is the thing worth avoiding.
#![allow(dead_code)]

use std::fs::File;
use std::future::poll_fn;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::task::Poll;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Hello, Node, ScanRequest};
use iris_format::{Builder, SchemaEncoding, SectionKind};
use iris_native::{Case, Corpus, Differential, Kernel, Mismatch, Native, Scanning};
use iris_runtime::{RESIDENT_TERMS, WINDOWED_TERMS, schema_to_ipc};
use iris_source::{Fetch, RangeSource, SourceError};
use iris_vm::{Handshake, RawBatch, Vm};

/// The workspace root, from this crate's manifest.
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is two levels above this crate")
}

/// The decoder modules these tests drive.
pub(crate) struct Modules {
    /// The decoder that reads the fixture.
    pub(crate) fixedwidth: Vec<u8>,
    /// The decoder that never returns.
    pub(crate) looping: Vec<u8>,
    /// The decoder with no header and one column.
    pub(crate) passthrough: Vec<u8>,
}

/// Compiles the decoders for wasm32, once per test binary.
pub(crate) fn modules() -> &'static Modules {
    static MODULES: OnceLock<Modules> = OnceLock::new();
    MODULES.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target").join("gate-wasm");

        // Held until the end of this block, so that no other test process is part way through a
        // build while this one reads what that build produced. See the note at the top of the file.
        std::fs::create_dir_all(&target_dir).expect("creating the nested target directory");
        let guard = File::create(target_dir.join("gate.lock")).expect("creating the build lock");
        guard.lock().expect("taking the build lock");

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
                "--example",
                "looping",
                "--example",
                "passthrough",
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
            "building the decoders for wasm32 failed. If the target is missing, run\n  \
             rustup target add wasm32-unknown-unknown\n\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let built = target_dir
            .join("wasm32-unknown-unknown")
            .join("release")
            .join("examples");
        let read = |name: &str| {
            let path = built.join(format!("{name}.wasm"));
            std::fs::read(&path)
                .unwrap_or_else(|err| panic!("cargo said it built {}: {err}", path.display()))
        };

        Modules {
            fixedwidth: read("fixedwidth"),
            looping: read("looping"),
            passthrough: read("passthrough"),
        }
    })
}

/// The decoder that reads the fixture.
pub(crate) fn decoder_module() -> &'static [u8] {
    &modules().fixedwidth
}

/// The decoder that agrees to everything and then spins forever on the first scan.
pub(crate) fn looping_module() -> &'static [u8] {
    &modules().looping
}

/// The decoder that reads one column of eight byte integers and has no header at all.
pub(crate) fn passthrough_module() -> &'static [u8] {
    &modules().passthrough
}

/// The fixed width decoder's header: how many rows, then how many columns.
pub(crate) const HEADER: u64 = 16;

/// The width of every value the fixtures hold.
pub(crate) const WIDTH: u64 = 8;

/// The value a fixture puts in a given cell.
///
/// The row index, offset by the column. Nothing cleverer, because the failure worth catching is a
/// read that came back holding the bytes from somewhere else, and a value derived from the row says
/// which row turned up instead.
pub(crate) fn cell(column: u64, row: u64) -> i64 {
    i64::try_from(column * 1_000_000_000 + row).expect("the fixture's values all fit")
}

/// The schema for a fixture of this many columns: non-nullable `i64`, one field each.
pub(crate) fn schema(columns: u64) -> Schema {
    Schema::new(
        (0..columns)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, false))
            .collect::<Vec<_>>(),
    )
}

/// The bytes the fixed width decoder reads: two `u64` of header, then column by column.
///
/// Filled in place rather than pushed onto, because the largest fixture is four gigabytes and a
/// `Vec` that doubles as it grows would peak at three copies of that. This allocates the whole thing
/// once and writes into it, which is the same reason [`Builder::build_into`] exists.
pub(crate) fn source(rows: u64, columns: u64) -> Vec<u8> {
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
pub(crate) fn builder(rows: u64, columns: u64) -> Builder {
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

/// The bytes the passthrough decoder reads: one column of values and no header.
///
/// The values are the same [`cell`] function column zero would produce, so a test that reads this
/// dataset and a test that reads the first column of a fixed width one are checking against the
/// same expected numbers.
pub(crate) fn flat_source(rows: u64) -> Vec<u8> {
    (0..rows)
        .flat_map(|row| cell(0, row).to_le_bytes())
        .collect()
}

/// A container the passthrough decoder reads, which is one column and no header.
///
/// A second decoder matters here rather than being more of the same. Both harnesses in
/// `tests/harness.rs` are claims about every decoder in the tree, and a claim checked against one
/// decoder is a claim about that decoder. This one asks for its rows differently: it caps a scan at
/// a thousand and twenty four rows of its own accord, reads no header, and offers no projection, so
/// it exercises the harnesses against a decoder that answers a request with less than was asked
/// for, which is legal and which a harness written around the first decoder would not have met.
pub(crate) fn flat_builder(rows: u64) -> Builder {
    let mut builder = Builder::new("flat", rows);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema(1)).expect("one integer column always encodes"),
    );
    builder.section(SectionKind::Data, flat_source(rows));
    builder.embed_decoder(
        "passthrough",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        passthrough_module().to_vec(),
    );
    builder
}

/// A file that removes itself, so a gate that fails does not leave four gigabytes behind.
///
/// Declared before the source that reads it in every test here, so it is dropped after that source
/// is closed. Removing a file somebody still has open is fine on Unix and is not on Windows, and
/// these tests run on both.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes `builder` to a file of its own under `dir` in the workspace target directory.
///
/// Under `target` and not under the system temporary directory, which on a lot of Linux machines is
/// a tmpfs. A four gigabyte fixture written to one of those is a four gigabyte fixture held in
/// memory, which is the thing the windowed path exists to avoid. Each gate names its own directory
/// so that a workflow can clean up after the one it ran.
pub(crate) fn write_container(dir: &str, name: &str, builder: &Builder) -> (Scratch, u64) {
    let dir = workspace_root().join("target").join(dir);
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

/// Asks a source for a range and waits for it without holding a thread.
///
/// A source that says it will wake the task is taken at its word, and one that says it will not is
/// asked again on the next poll, which is the arrangement `RangeSource::wake_when_ready` describes.
/// Nothing here actually goes pending, since every fixture these tests use is resident, and it is
/// written this way because a native implementation that spun here would give back the property M6
/// was about.
pub(crate) async fn read(
    source: &mut (dyn RangeSource + Send),
    at: u64,
    len: usize,
) -> Result<Vec<u8>, SourceError> {
    poll_fn(|cx| {
        match source.range(at, len) {
            Err(err) => return Poll::Ready(Err(err)),
            Ok(Fetch::Ready(bytes)) => return Poll::Ready(Ok(bytes.to_vec())),
            // Pending, and anything a later version of the trait adds, means come back later.
            Ok(_) => {}
        }
        if !source.wake_when_ready(cx.waker()) {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

/// What the fixed width module answers a [`Hello`] with.
///
/// A native implementation of it has to answer the same way, down to the decoder id, because the
/// host negotiates against this and a substitution that negotiated differently would be a different
/// decoder. The differential run compares the two answers before it compares a single row.
pub(crate) fn fixedwidth_handshake() -> Handshake {
    Handshake {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        required: CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        optional: CapabilitySet::new().with(Capability::PROJECTION),
        decoder_id: "fixedwidth".to_owned(),
    }
}

/// Decodes the fixed width layout the way `crates/iris-decoder/examples/fixedwidth.rs` decodes it.
///
/// Written from the module rather than from the description of the format, because the thing being
/// reproduced is not "the right values" but "the same bytes in the same batches". The two places
/// that is easy to get wrong are both here: the rows are cut into batches of `max_batch_rows`, which
/// the module takes from the session it opened with and this takes from the [`Hello`] it is handed,
/// and every array carries an empty validity buffer before its values, because the schema decides
/// how many buffers there are and leaving one out would shift every buffer after it.
pub(crate) async fn fixedwidth_batches(
    hello: &Hello,
    request: &ScanRequest<'_>,
    source: &mut (dyn RangeSource + Send),
) -> iris_native::Result<Vec<RawBatch>> {
    let header = read(source, 0, 16).await?;
    let rows = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
    let columns = u64::from_le_bytes(header[8..].try_into().expect("eight bytes"));

    // The module refuses a header describing more data than the source holds, and it refuses it when
    // it opens rather than partway through a batch. This is the same refusal one call later, which
    // is as close as a native implementation gets: there is no open here to refuse from.
    let declared = rows
        .checked_mul(columns)
        .and_then(|values| values.checked_mul(WIDTH))
        .and_then(|bytes| bytes.checked_add(HEADER))
        .ok_or_else(|| {
            iris_native::Error::malformed("the header describes more bytes than exist anywhere")
        })?;
    if hello.source_bytes != 0 && declared > hello.source_bytes {
        return Err(iris_native::Error::malformed(
            "the header describes more rows than the source has bytes for",
        ));
    }

    let wanted_columns: Vec<u64> = if request.projection.is_empty() {
        (0..columns).collect()
    } else {
        request.projection.iter().map(u64::from).collect()
    };
    if wanted_columns.iter().any(|&column| column >= columns) {
        return Err(iris_native::Error::malformed(
            "the projection names a column this dataset does not have",
        ));
    }

    let batch_rows = hello.max_batch_rows.max(1);
    let start = request.row_start.min(rows);
    let wanted = request.row_count.min(rows - start);

    let mut batches = Vec::new();
    let mut done = 0;
    while done < wanted {
        let count = batch_rows.min(wanted - done);
        let mut batch = RawBatch {
            rows: count,
            nodes: Vec::new(),
            buffers: Vec::new(),
        };
        for &column in &wanted_columns {
            let at = HEADER + (column * rows + start + done) * WIDTH;
            let len = usize::try_from(count * WIDTH).expect("a fixture batch fits in memory");
            batch.nodes.push(Node {
                length: count,
                null_count: 0,
            });
            batch.buffers.push(Vec::new());
            batch.buffers.push(read(source, at, len).await?);
        }
        batches.push(batch);
        done += count;
    }
    Ok(batches)
}

/// What [`FixedWidth`] calls itself in a log.
///
/// A constant rather than a literal in the impl, because the tests that read a log line have to
/// assert on the same string the implementation wrote, and two copies of it would drift.
pub(crate) const FIXEDWIDTH_IDENTITY: &str = "fixedwidth-test 1.0.0";

/// A native implementation of the fixed width decoder that agrees with it.
#[derive(Debug)]
pub(crate) struct FixedWidth;

impl Native for FixedWidth {
    fn identity(&self) -> &'static str {
        FIXEDWIDTH_IDENTITY
    }

    fn handshake(&self, _hello: &Hello) -> iris_native::Result<Handshake> {
        Ok(fixedwidth_handshake())
    }

    fn scan<'a>(
        &'a self,
        hello: &'a Hello,
        request: &'a ScanRequest<'a>,
        source: &'a mut (dyn RangeSource + Send),
    ) -> Scanning<'a> {
        Box::pin(fixedwidth_batches(hello, request, source))
    }
}

/// The rows in the largest case [`native_corpus`] holds.
///
/// Named because more than one test turns on it. A container larger than this is a dataset the
/// corpus never covered, which is what makes the honest residual demonstrable rather than a remark.
pub(crate) const CORPUS_ROWS: u64 = 97;

/// The datasets a native fixed width kernel is proved against.
///
/// Three shapes rather than one large one. A single column dataset is where an implementation that
/// confuses the row stride with the column stride still passes, a three column one is where the
/// projection order shows, and a one row one is where the ends of a range meet in the middle. None
/// of them is the shape of the container the tests then read, which is deliberate: a corpus that
/// happened to contain the exact dataset under test would prove more here than a corpus proves in
/// general.
pub(crate) fn native_corpus() -> Corpus {
    Corpus::new()
        .with_case(Case::new("one row, one column", source(1, 1), 1, 1))
        .with_case(Case::new(
            "a single column of readings",
            source(CORPUS_ROWS, 1),
            CORPUS_ROWS,
            1,
        ))
        .with_case(Case::new("three columns", source(64, 3), 64, 3))
}

/// Runs a differential of this implementation against the fixed width module, and reports what came
/// of it.
///
/// The engine is built here and dropped here. A [`Kernel`] borrows nothing from it, so a caller ends
/// up holding the result of a run rather than the machinery of one.
pub(crate) fn attempt(
    native: Arc<dyn Native>,
    corpus: &Corpus,
    terms: &[CapabilitySet],
) -> Result<Kernel, Mismatch> {
    let vm = Vm::new().expect("a compiler starts");
    let mut run = Differential::new(&vm, decoder_module());
    for &set in terms {
        run = run.offering(set);
    }
    run.verify(native, corpus)
}

/// The same run, under both sets of terms this host offers, for a kernel expected to agree.
pub(crate) fn prove(native: Arc<dyn Native>) -> Kernel {
    attempt(native, &native_corpus(), &[RESIDENT_TERMS, WINDOWED_TERMS])
        .expect("this implementation agrees with the module on every case in the corpus")
}

/// Every value in a column of the batches, in order.
pub(crate) fn column_values(batches: &[RecordBatch], column: usize) -> Vec<i64> {
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
