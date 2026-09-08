//! The DuckDB extension for iris.
//!
//! It adds one table function. `select * from iris_scan('c.iris')` opens a container, compiles its
//! decoder if this process has not seen it before, and hands the batches to DuckDB. There is no
//! reader for the iris format in DuckDB and this file does not add one, which is the whole point:
//! the container carries its own decoder and the extension is the twenty lines that let an engine
//! ask for it.
//!
//! Nothing here links DuckDB. The `loadable-extension` feature reaches every DuckDB function
//! through the table of pointers DuckDB hands the extension when it loads it, so what this crate
//! produces is a shared library that names no DuckDB symbol and can be built on a machine with no
//! DuckDB on it.
//!
//! # Two Arrows
//!
//! The `duckdb` crate is built against Arrow 58 and the rest of this workspace is on Arrow 59, so
//! the batches iris produces are not, to the compiler, the batches DuckDB accepts. That would be a
//! wall in most projects. Here it is a two line function, because what crosses between them is an
//! `ArrowSchema` and an `ArrowArrayStream` from the Arrow C data interface, and those are C
//! structures with a layout the specification fixes rather than Rust types whose layout a compiler
//! picks. Both crates declare them `#[repr(C)]` with the same fields in the same order, so a value
//! of one is a value of the other.
//!
//! This is the argument iris makes about file formats, made about a Rust dependency graph, and it
//! is worth saying out loud: a version skew that would normally mean forking a crate or waiting for
//! an upstream release costs nothing at all, because the boundary was specified as a byte layout
//! instead of as a set of types.
//!
//! # Building and loading it
//!
//! `ci/duckdb-extension.sh` builds the library and appends the metadata block DuckDB reads before
//! it will load anything. The result is unsigned, so a session that loads it has to say so:
//!
//! ```text
//! duckdb -unsigned -c "load 'iris.duckdb_extension'; select * from iris_scan('c.iris')"
//! ```
//!
//! `docs/DUCKDB.md` is the guide.

// The entry point macro at the bottom of this file writes two more functions next to the one it is
// attached to and gives them its span, so a lint about either of them is reported against a line
// nobody wrote and there is nowhere to put an attribute. One of them returns a `Result` and is
// documented with what it is for rather than with what it can fail with, and the entry point takes
// its connection by value because passing it by value is what the macro does. Neither of these can
// be allowed any closer to the code than here.
#![allow(clippy::missing_errors_doc, clippy::needless_pass_by_value)]

use std::env;
use std::error::Error;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use duckdb::arrow::datatypes::Schema;
use duckdb::arrow::ffi::FFI_ArrowSchema as DuckSchema;
use duckdb::arrow::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream as DuckStream};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::ffi::duckdb_vector_size;
use duckdb::vtab::{
    BindInfo, InitInfo, TableFunctionInfo, VTab, record_batch_to_duckdb_data_chunk,
    to_duckdb_logical_type_for_field,
};
use duckdb::{Connection, duckdb_entrypoint_c_api};
use iris::{
    FFI_ArrowArrayStream as IrisStream, FFI_ArrowSchema as IrisSchema, IrisDataset, IrisRuntime,
};

/// The directory compiled decoders are kept in between runs, if a session names one.
const COMPILATION_CACHE: &str = "IRIS_COMPILATION_CACHE";

/// What a chunk holds if DuckDB is not able to say, which it always is.
const STANDARD_VECTOR_SIZE: usize = 2048;

/// The runtime every scan in this process opens through.
///
/// One of these is what makes the second query against a container cheap: the decoder compiled for
/// the first is still in its pool. A runtime per bind would compile the same module again for every
/// statement, which is the cost this project exists to avoid.
fn runtime() -> Result<&'static IrisRuntime, Box<dyn Error>> {
    static RUNTIME: OnceLock<IrisRuntime> = OnceLock::new();

    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }

    let mut built = IrisRuntime::new()?;
    // Off unless a session names a directory, and worth naming. With it the first query a process
    // runs against a container it has seen in an earlier process skips compilation too, rather than
    // only the queries after the first.
    if let Ok(dir) = env::var(COMPILATION_CACHE) {
        built.set_compilation_cache(&dir)?;
    }

    // Two threads binding at once both build one and the loser's is dropped, which costs a
    // compilation nobody uses and is worth more than a lock held across a wasm module build.
    Ok(RUNTIME.get_or_init(|| built))
}

/// Moves an Arrow C schema from the Arrow iris was built against to the Arrow DuckDB was.
fn cross_schema(schema: IrisSchema) -> DuckSchema {
    // SAFETY: both types are the `ArrowSchema` of the Arrow C data interface, declared `#[repr(C)]`
    // in both crates with the same fields in the same order, because the specification fixes them.
    // The value is moved rather than copied, so the release callback is called exactly once and by
    // the same code that would have called it before.
    unsafe { std::mem::transmute::<IrisSchema, DuckSchema>(schema) }
}

/// Moves an Arrow C stream from the Arrow iris was built against to the Arrow DuckDB was.
fn cross_stream(stream: IrisStream) -> DuckStream {
    // SAFETY: as in `cross_schema` above, and for the same reason. This is `ArrowArrayStream`.
    unsafe { std::mem::transmute::<IrisStream, DuckStream>(stream) }
}

/// The table function.
struct IrisScan;

/// What bind worked out, which is the container and how wide it is.
struct Bound {
    /// Open, with its schema and its name already read. Scanning it opens it again, which is what
    /// `iris-c` documents and what makes this shareable across the threads DuckDB runs the scan on.
    dataset: IrisDataset,
    /// How many columns the container has, so a projection can be checked against it.
    width: usize,
}

/// What init worked out, which is the projection, plus the batches once something has asked.
struct Scan {
    /// Positions in the container's schema, in the order DuckDB wants them in the chunk. Empty
    /// means the query wants no columns at all, which is what `count(*)` asks for.
    columns: Vec<u32>,
    /// Filled by the first call rather than by init, so that a bind and an init that a planner
    /// throws away do not decode anything.
    batches: Mutex<Option<Batches>>,
}

/// The result of a scan, being handed out a chunk at a time.
struct Batches {
    /// Reversed, so that finishing one is a pop rather than a shift.
    remaining: Vec<RecordBatch>,
    /// How far into the last of `remaining` the previous call got.
    offset: usize,
}

impl Batches {
    /// Scans the container and keeps what came back.
    ///
    /// The scan happens here in full. iris decodes eagerly, so a stream over it is a queue rather
    /// than work in progress, and draining it now means nothing after this point can fail for a
    /// reason that has to do with iris.
    fn open(dataset: &IrisDataset, columns: &[u32]) -> Result<Self, Box<dyn Error>> {
        let stream = cross_stream(dataset.stream(columns)?);
        let reader = ArrowArrayStreamReader::try_new(stream)?;
        let mut remaining = reader.collect::<Result<Vec<_>, _>>()?;
        remaining.reverse();
        Ok(Self {
            remaining,
            offset: 0,
        })
    }
}

impl VTab for IrisScan {
    type BindData = Bound;
    type InitData = Scan;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        let path = bind.get_parameter(0).to_string();
        let dataset = runtime()?.open_path(Path::new(&path))?;

        // The container's schema, crossed into DuckDB's Arrow and turned into DuckDB types. Every
        // column is declared here whatever the query asked for, because bind is where the shape of
        // the table is decided and the projection is not known until init.
        let schema = Schema::try_from(&cross_schema(dataset.ffi_schema()?))?;
        for field in schema.fields() {
            bind.add_result_column(field.name(), to_duckdb_logical_type_for_field(field)?);
        }

        let width = schema.fields().len();
        Ok(Bound { dataset, width })
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        let mut columns = Vec::new();
        for index in init.get_column_indices() {
            columns.push(u32::try_from(index)?);
        }

        Ok(Scan {
            columns,
            batches: Mutex::new(None),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        let bound = func.get_bind_data();
        let scan = func.get_init_data();

        // A query that wants no columns still wants a count, so one column is read to find out how
        // many rows there are and none of it is written out. Asking iris for nothing means asking
        // for everything, which would read the whole container to answer `count(*)`.
        let counting = scan.columns.is_empty() && bound.width > 0;
        let wanted: &[u32] = if counting { &[0] } else { &scan.columns };
        for column in wanted {
            if usize::try_from(*column)? >= bound.width {
                return Err(format!(
                    "this query asked for column {column} of a container that has {} of them",
                    bound.width
                )
                .into());
            }
        }

        let mut held = scan
            .batches
            .lock()
            .map_err(|_| "an earlier chunk of this scan panicked and took its state with it")?;
        if held.is_none() {
            *held = Some(Batches::open(&bound.dataset, wanted)?);
        }
        let batches = held.as_mut().expect("the line above put one there");

        // SAFETY: the API table this reads through was installed by the entry point before DuckDB
        // called any callback in this file, which is what makes every DuckDB call here work.
        let limit =
            usize::try_from(unsafe { duckdb_vector_size() }).unwrap_or(STANDARD_VECTOR_SIZE);

        loop {
            let Some(batch) = batches.remaining.last() else {
                output.set_len(0);
                return Ok(());
            };

            let left = batch.num_rows() - batches.offset;
            if left == 0 {
                batches.remaining.pop();
                batches.offset = 0;
                continue;
            }

            // A chunk holds a fixed number of rows and a batch is whatever the decoder produced, so
            // a batch is handed over a slice at a time and the offset is where the last one stopped.
            let take = left.min(limit);
            if counting {
                output.set_len(take);
            } else {
                record_batch_to_duckdb_data_chunk(&batch.slice(batches.offset, take), output)?;
            }
            batches.offset += take;
            return Ok(());
        }
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)])
    }

    fn supports_pushdown() -> bool {
        true
    }
}

/// Registers `iris_scan` on a connection, which is what DuckDB calls when it loads this library.
///
/// # Errors
///
/// If the table function cannot be registered, which means a function of that name is already
/// there.
#[duckdb_entrypoint_c_api(ext_name = "iris", min_duckdb_version = "v1.2.0")]
pub fn entrypoint(con: Connection) -> Result<(), Box<dyn Error>> {
    con.register_table_function::<IrisScan>("iris_scan")?;
    Ok(())
}
