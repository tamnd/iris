//! The containers these tests query, and the decoders that read them.
//!
//! The decoders are compiled from `crates/iris-decoder/examples` rather than checked in, for the
//! reason `iris-runtime/tests/support/mod.rs` gives at length: a committed `.wasm` fixture is a
//! binary nobody reads that keeps passing after the source it came from has stopped matching it.
//!
//! It builds into the same directory that module uses, and takes the same lock, so the two test
//! suites share one build rather than racing over two. Cargo's own lock is per target directory and
//! covers the build; it does not cover the reads afterwards, and cargo finishes a build by removing
//! the example and linking it again even when nothing needed compiling. The lock here is held across
//! the build and the reads together, which is the only arrangement where what was built is still
//! there to be read.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet};
use iris_format::{Builder, SchemaEncoding, SectionKind};
use iris_runtime::schema_to_ipc;

/// The fixed width decoder's header: how many rows, then how many columns.
const HEADER: usize = 16;

/// The workspace root, from this crate's manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is two levels above this crate")
}

/// The decoder modules these tests drive.
struct Modules {
    /// Reads a header and any number of columns, and does projection.
    fixedwidth: Vec<u8>,
    /// Reads one column, has no header, and does not do projection.
    passthrough: Vec<u8>,
}

/// Compiles the decoders for wasm32, once per test binary.
fn modules() -> &'static Modules {
    static MODULES: OnceLock<Modules> = OnceLock::new();
    MODULES.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target").join("gate-wasm");

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
            passthrough: read("passthrough"),
        }
    })
}

/// The value a fixture puts in a given cell.
///
/// The row index, offset by the column, so a value says which row and which column it came from and
/// a batch assembled from the wrong offset is wrong by a whole column rather than by nothing.
pub(crate) fn cell(column: u64, row: u64) -> i64 {
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
fn source(rows: u64, columns: u64) -> Vec<u8> {
    let values = usize::try_from(rows * columns).expect("the fixture fits in this host's memory");
    let mut out = vec![0u8; HEADER + values * 8];
    out[..8].copy_from_slice(&rows.to_le_bytes());
    out[8..16].copy_from_slice(&columns.to_le_bytes());

    let mut at = HEADER;
    for column in 0..columns {
        for row in 0..rows {
            out[at..at + 8].copy_from_slice(&cell(column, row).to_le_bytes());
            at += 8;
        }
    }
    out
}

/// A container of this shape, carrying the decoder that reads it and does projection.
pub(crate) fn container(rows: u64, columns: u64) -> Vec<u8> {
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
        modules().fixedwidth.clone(),
    );
    builder.build().expect("the container is writable")
}

/// A container the passthrough decoder reads, which is one column and no projection.
///
/// The values are the same [`cell`] function column zero produces, so a test that reads this and a
/// test that reads the first column of a fixed width container check against the same numbers.
pub(crate) fn flat_container(rows: u64) -> Vec<u8> {
    let mut builder = Builder::new("flat", rows);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema(1)).expect("one integer column always encodes"),
    );
    builder.section(
        SectionKind::Data,
        (0..rows)
            .flat_map(|row| cell(0, row).to_le_bytes())
            .collect::<Vec<u8>>(),
    );
    builder.embed_decoder(
        "passthrough",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        modules().passthrough.clone(),
    );
    builder.build().expect("the container is writable")
}

/// A file that removes itself, so a test that fails does not leave a fixture behind.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes a container to a file of its own under the workspace target directory.
///
/// Under `target` and not under the system temporary directory, which on a lot of Linux machines is
/// a tmpfs. A fixture written to one of those is a fixture held in memory, which is the thing the
/// windowed path exists to avoid and the thing these tests are counting the reads of.
pub(crate) fn write(name: &str, bytes: &[u8]) -> Scratch {
    let dir = workspace_root().join("target").join("df-query");
    std::fs::create_dir_all(&dir).expect("creating the fixture directory");
    let path = dir.join(format!("{name}.iris"));
    let scratch = Scratch(path.clone());

    let file = File::create(&path).expect("creating the fixture");
    let mut out = BufWriter::new(file);
    out.write_all(bytes).expect("writing the fixture");
    out.flush().expect("flushing the fixture");
    out.into_inner()
        .expect("the fixture is written")
        .sync_all()
        .expect("the fixture reaches the disk");
    scratch
}

/// Every value in a column of the batches, sorted.
///
/// Sorted because the partitions of one query finish in whatever order they finish in, so the order
/// values arrive in is not something a test should be asserting on. What each test here is checking
/// is that every row turned up exactly once and carried the value it was written with.
pub(crate) fn values(batches: &[RecordBatch], column: usize) -> Vec<i64> {
    let mut out: Vec<i64> = batches
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
        .collect();
    out.sort_unstable();
    out
}
