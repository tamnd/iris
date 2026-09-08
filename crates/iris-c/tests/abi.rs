//! The C entry points, driven the way a C caller drives them.
//!
//! These call the exported functions directly rather than going through a linker, which is what the
//! `rlib` in the manifest is for. What that buys is that the argument checking, the ownership rules
//! and the error path are all exercised by `cargo test` on every platform CI runs on, so the C smoke
//! test in `.github/workflows/package.yml` is left to prove the one thing this cannot: that the
//! artefact links and runs on a machine with no Rust on it.
//!
//! The fixture comes from the gate tests next door, so the container these open is the same
//! container the rest of the suite opens.

use std::ffi::{CStr, CString, c_char};
use std::ptr;

use arrow_array::RecordBatchReader as _;
use arrow_array::ffi::FFI_ArrowSchema;
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_schema::Schema;
use iris::{
    IRIS_ERROR, IRIS_INVALID, IRIS_OK, IrisDataset, IrisRuntime, iris_dataset_free,
    iris_dataset_name, iris_dataset_scan, iris_dataset_scan_columns, iris_dataset_schema,
    iris_open, iris_open_path, iris_runtime_free, iris_runtime_new,
    iris_runtime_set_compilation_cache, iris_string_free, iris_version,
};

#[path = "../../iris-runtime/tests/support/mod.rs"]
mod support;

use support::builder;

/// Rows in the fixture these open. Counted the way the assertions want to count them, which is why
/// this is a `usize` and the builder next door is handed a `u64`.
const ROWS: usize = 256;

/// Columns in the fixture these open.
const COLUMNS: usize = 3;

/// The fixture, built once per test the way every other gate test builds it.
fn container() -> Vec<u8> {
    builder(ROWS as u64, COLUMNS as u64)
        .build()
        .expect("a container this size fits")
}

/// A runtime handle, or a panic naming what could not be built.
fn runtime() -> *mut IrisRuntime {
    let runtime = iris_runtime_new();
    assert!(!runtime.is_null(), "the runtime could not be built");
    runtime
}

/// Opens bytes and returns the handle, asserting nothing went wrong on the way.
fn open(runtime: *mut IrisRuntime, bytes: &[u8]) -> *mut IrisDataset {
    let mut dataset = ptr::null_mut();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, a slice that outlives the call, and one writable pointer each.
    let status = unsafe {
        iris_open(
            runtime,
            bytes.as_ptr(),
            bytes.len(),
            &raw mut dataset,
            &raw mut error,
        )
    };
    assert_eq!(status, IRIS_OK, "{}", taken(error));
    assert!(!dataset.is_null());
    dataset
}

/// The message behind an error pointer, released on the way out.
fn taken(error: *mut c_char) -> String {
    if error.is_null() {
        return "no message".to_owned();
    }
    // SAFETY: this library wrote it and nothing has freed it.
    let message = unsafe { CStr::from_ptr(error) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: same pointer, and the copy above is done with it.
    unsafe { iris_string_free(error) };
    message
}

/// The schema behind a dataset handle.
fn schema_of(dataset: *mut IrisDataset) -> Schema {
    let mut schema = FFI_ArrowSchema::empty();
    let mut error = ptr::null_mut();
    // SAFETY: a live dataset, one writable schema, one writable pointer.
    let status = unsafe { iris_dataset_schema(dataset, &raw mut schema, &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));
    Schema::try_from(&schema).expect("the schema crosses back")
}

/// Rows and batches a stream produced.
fn drain(stream: FFI_ArrowArrayStream) -> (usize, usize) {
    let reader = ArrowArrayStreamReader::try_new(stream).expect("the stream is well formed");
    let mut rows = 0;
    let mut batches = 0;
    for batch in reader {
        let batch = batch.expect("every batch was decoded before the stream was handed over");
        rows += batch.num_rows();
        batches += 1;
    }
    (rows, batches)
}

#[test]
fn the_version_is_the_one_this_was_built_from() {
    // SAFETY: the pointer is static and nul terminated, which is what this function promises.
    let version = unsafe { CStr::from_ptr(iris_version()) };
    assert_eq!(version.to_str().expect("ascii"), env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_container_in_memory_opens_and_scans() {
    let bytes = container();
    let runtime = runtime();
    let dataset = open(runtime, &bytes);

    // SAFETY: a live dataset, and the borrow ends before it is freed.
    let name = unsafe { CStr::from_ptr(iris_dataset_name(dataset)) };
    assert!(!name.to_bytes().is_empty(), "a container carries a name");

    let schema = schema_of(dataset);
    assert_eq!(schema.fields().len(), COLUMNS);

    let mut stream = FFI_ArrowArrayStream::empty();
    let mut error = ptr::null_mut();
    // SAFETY: a live dataset, one writable stream, one writable pointer.
    let status = unsafe { iris_dataset_scan(dataset, &raw mut stream, &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));

    let (rows, batches) = drain(stream);
    assert_eq!(rows, ROWS);
    assert!(batches > 0);

    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(runtime);
    }
}

#[test]
fn a_container_in_a_file_opens_the_same_way() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("sample.iris");
    std::fs::write(&path, container()).expect("the fixture is written");
    let path = CString::new(path.to_str().expect("utf8")).expect("no nul in a temporary path");

    let runtime = runtime();
    let mut dataset = ptr::null_mut();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, a nul terminated path, one writable pointer each.
    let status =
        unsafe { iris_open_path(runtime, path.as_ptr(), &raw mut dataset, &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));

    assert_eq!(schema_of(dataset).fields().len(), COLUMNS);

    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(runtime);
    }
}

#[test]
fn a_projection_reads_the_columns_it_named() {
    let bytes = container();
    let runtime = runtime();
    let dataset = open(runtime, &bytes);

    let columns = [2_u32, 0];
    let mut stream = FFI_ArrowArrayStream::empty();
    let mut error = ptr::null_mut();
    // SAFETY: a live dataset, two readable positions, one writable stream and one writable pointer.
    let status = unsafe {
        iris_dataset_scan_columns(
            dataset,
            columns.as_ptr(),
            columns.len(),
            &raw mut stream,
            &raw mut error,
        )
    };
    assert_eq!(status, IRIS_OK, "{}", taken(error));

    let reader = ArrowArrayStreamReader::try_new(stream).expect("the stream is well formed");
    let schema = reader.schema();
    assert_eq!(schema.fields().len(), columns.len());

    let whole = schema_of(dataset);
    assert_eq!(schema.field(0).name(), whole.field(2).name());
    assert_eq!(schema.field(1).name(), whole.field(0).name());

    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(runtime);
    }
}

#[test]
fn a_count_of_zero_is_every_column() {
    let bytes = container();
    let runtime = runtime();
    let dataset = open(runtime, &bytes);

    let mut stream = FFI_ArrowArrayStream::empty();
    let mut error = ptr::null_mut();
    // SAFETY: a live dataset, a null column list with a count of zero, which the contract allows.
    let status = unsafe {
        iris_dataset_scan_columns(dataset, ptr::null(), 0, &raw mut stream, &raw mut error)
    };
    assert_eq!(status, IRIS_OK, "{}", taken(error));

    let reader = ArrowArrayStreamReader::try_new(stream).expect("the stream is well formed");
    assert_eq!(reader.schema().fields().len(), COLUMNS);

    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(runtime);
    }
}

#[test]
fn bytes_that_are_not_a_container_come_back_as_a_message() {
    let runtime = runtime();
    let rubbish = [0_u8; 64];
    let mut dataset = ptr::null_mut();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, sixty four readable bytes, one writable pointer each.
    let status = unsafe {
        iris_open(
            runtime,
            rubbish.as_ptr(),
            rubbish.len(),
            &raw mut dataset,
            &raw mut error,
        )
    };
    assert_eq!(status, IRIS_ERROR);
    assert!(dataset.is_null(), "nothing is written on the way out");
    assert_ne!(taken(error), "no message");

    // SAFETY: the handle came from this test and has not been freed.
    unsafe { iris_runtime_free(runtime) };
}

#[test]
fn a_caller_that_does_not_want_the_message_still_gets_the_status() {
    let runtime = runtime();
    let rubbish = [0_u8; 64];
    let mut dataset = ptr::null_mut();
    // SAFETY: a live runtime, sixty four readable bytes, one writable pointer, and a null error,
    // which the contract allows.
    let status = unsafe {
        iris_open(
            runtime,
            rubbish.as_ptr(),
            rubbish.len(),
            &raw mut dataset,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, IRIS_ERROR);

    // SAFETY: the handle came from this test and has not been freed.
    unsafe { iris_runtime_free(runtime) };
}

#[test]
fn a_file_that_is_not_there_names_itself() {
    let runtime = runtime();
    let path = CString::new("no-such-container.iris").expect("no nul");
    let mut dataset = ptr::null_mut();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, a nul terminated path, one writable pointer each.
    let status =
        unsafe { iris_open_path(runtime, path.as_ptr(), &raw mut dataset, &raw mut error) };
    assert_eq!(status, IRIS_ERROR);
    assert!(taken(error).contains("no-such-container.iris"));

    // SAFETY: the handle came from this test and has not been freed.
    unsafe { iris_runtime_free(runtime) };
}

#[test]
fn a_null_argument_is_refused_without_a_message() {
    let bytes = container();
    let mut dataset = ptr::null_mut();
    let mut error = ptr::null_mut();
    // SAFETY: a null runtime, which is exactly what this is checking.
    let status = unsafe {
        iris_open(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len(),
            &raw mut dataset,
            &raw mut error,
        )
    };
    assert_eq!(status, IRIS_INVALID);
    assert!(error.is_null(), "a caller's bug produces no message");

    // SAFETY: null is accepted by every free here and does nothing.
    unsafe {
        iris_dataset_free(ptr::null_mut());
        iris_runtime_free(ptr::null_mut());
        iris_string_free(ptr::null_mut());
    }
}

#[test]
fn a_dataset_outlives_the_runtime_handle_it_came_from() {
    let bytes = container();
    let runtime = runtime();
    let dataset = open(runtime, &bytes);

    // SAFETY: the handle came from this test and has not been freed.
    unsafe { iris_runtime_free(runtime) };

    // The scan below is the point: it opens the container again through the runtime the dataset is
    // holding, which is the one the handle above used to point at.
    let mut stream = FFI_ArrowArrayStream::empty();
    let mut error = ptr::null_mut();
    // SAFETY: a live dataset, one writable stream, one writable pointer.
    let status = unsafe { iris_dataset_scan(dataset, &raw mut stream, &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));
    assert_eq!(drain(stream).0, ROWS);

    // SAFETY: the handle came from this test and has not been freed.
    unsafe { iris_dataset_free(dataset) };
}

#[test]
fn a_compilation_cache_is_named_through_the_abi() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = CString::new(dir.path().to_str().expect("utf8")).expect("no nul");
    let bytes = container();

    let first = runtime();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, a nul terminated directory, one writable pointer.
    let status =
        unsafe { iris_runtime_set_compilation_cache(first, path.as_ptr(), &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));
    let dataset = open(first, &bytes);
    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(first);
    }

    assert!(
        std::fs::read_dir(dir.path())
            .expect("the directory is readable")
            .next()
            .is_some(),
        "the first open wrote an artefact"
    );

    // A second runtime over the same directory, which is what a second process would be.
    let second = runtime();
    let mut error = ptr::null_mut();
    // SAFETY: a live runtime, a nul terminated directory, one writable pointer.
    let status =
        unsafe { iris_runtime_set_compilation_cache(second, path.as_ptr(), &raw mut error) };
    assert_eq!(status, IRIS_OK, "{}", taken(error));
    let dataset = open(second, &bytes);
    assert_eq!(schema_of(dataset).fields().len(), COLUMNS);
    // SAFETY: both handles came from this test and neither has been freed.
    unsafe {
        iris_dataset_free(dataset);
        iris_runtime_free(second);
    }
}
