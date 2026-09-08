//! The C ABI for iris.
//!
//! Everything a C caller gets back that has structure in it is an Arrow C structure. A schema is an
//! `ArrowSchema` and a scan is an `ArrowArrayStream`, both defined by the Arrow C data interface,
//! which means the types here are five opaque handles and nothing else. That is a deliberate refusal
//! to invent a column representation: anything that can already read Arrow can read the output of
//! this library without a line of glue, and anything that cannot is not a consumer this ABI should
//! be designing for.
//!
//! `include/iris.h` is the header, `docs/C_ABI.md` is the guide, and `examples/scan.c` is a complete
//! program that opens a container and prints what it holds.
//!
//! # How a call reports failure
//!
//! Every fallible entry point returns an `int32_t` status and takes a `char **error` as its last
//! argument. On [`IRIS_ERROR`] the message is written there, owned by the caller, and released with
//! [`iris_string_free`]. On [`IRIS_OK`] nothing is written to it. A caller that does not want the
//! message passes null and still gets the status.
//!
//! The usual shape for this is a `iris_last_error()` reading a thread local, and it is not what
//! happens here. `ci/discipline.py` refuses thread locals anywhere in this tree, because a decode
//! job is a thing that moves between threads and state it carries without owning is state it loses
//! when it moves. A last error slot is exactly that, so the message is handed back with the call
//! that produced it instead, which is also the only version that behaves when two threads are
//! opening two containers.
//!
//! # What owns what
//!
//! A handle from this library is freed by this library, and a pointer into this library's memory
//! stays valid until the handle it came from is freed. There are four rules and they are the whole
//! memory model:
//!
//! - [`iris_runtime_new`] gives a handle that [`iris_runtime_free`] releases.
//! - [`iris_open`] gives a handle that [`iris_dataset_free`] releases. It copies the bytes it is
//!   given, so the caller may free them the moment the call returns.
//! - A message written to an `error` argument is released with [`iris_string_free`].
//! - An `ArrowSchema` or an `ArrowArrayStream` this library fills in is released by calling its own
//!   `release` member, which is what the Arrow C data interface says and not a rule of this library.
//!
//! A dataset keeps the runtime it was opened from alive, so freeing the runtime handle first is
//! allowed rather than being a use after free waiting to happen. The order that reads best is still
//! datasets first.
//!
//! # Why a scan opens the container again
//!
//! A `Dataset` in Rust borrows the bytes it was opened over, and a borrow is not a thing that
//! crosses a C boundary. Holding one behind an opaque handle means a struct that points into itself,
//! which is sound only by an argument nobody reviewing this file should have to check. So the handle
//! keeps the bytes and the runtime, and each call opens the container again.
//!
//! What makes that an easy trade rather than a cost is the decoder pool. The second open of a
//! container in one process finds its decoder already compiled and takes about four hundredths of a
//! millisecond, which `docs/COLD_START.md` measures. The schema and the name are read once at
//! [`iris_open`] and kept, so the calls that return them do no work at all.

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use arrow_array::ffi::FFI_ArrowSchema;
use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, SchemaRef};
use iris_runtime::Runtime;

/// The call succeeded and nothing was written to the error argument.
pub const IRIS_OK: i32 = 0;

/// The call failed and the reason is in the error argument, unless it was null.
pub const IRIS_ERROR: i32 = 1;

/// An argument the call cannot do without was null, so nothing was attempted and no message was
/// produced. This is a bug in the caller rather than a condition, which is why it is separate.
pub const IRIS_INVALID: i32 = 2;

/// A runtime, which is where compiled decoders are held.
///
/// One is enough for a process. Every dataset opened from it shares its decoder pool, and its
/// compilation cache if one was named, which is what makes the second open of a container cheap.
#[derive(Debug)]
pub struct IrisRuntime {
    /// Shared rather than owned outright, so that a dataset outliving its runtime handle is a
    /// supported order rather than a crash.
    runtime: Arc<Runtime>,
}

/// An open container.
///
/// It holds the bytes, the runtime it was opened from, and the two things read once on the way in.
#[derive(Debug)]
pub struct IrisDataset {
    /// The runtime, kept alive for as long as this handle is.
    runtime: Arc<Runtime>,
    /// The container. Copied at [`iris_open`], because a C caller's buffer is not something this
    /// library can make promises about the lifetime of.
    bytes: Box<[u8]>,
    /// The name in the container, held as a C string so that returning it is not an allocation.
    name: CString,
    /// The schema, read once.
    schema: SchemaRef,
}

/// Batches that have already been decoded, handed out one at a time.
///
/// The Arrow C stream interface wants something that yields batches, and a scan produces all of
/// them at once, so this is the adapter between the two. It is not laziness pretending to be a
/// stream: by the time a caller has one of these the work is done, and `docs/C_ABI.md` says so
/// rather than leaving a C programmer to infer it from the word stream.
#[derive(Debug)]
struct Decoded {
    /// The schema every batch below shares.
    schema: SchemaRef,
    /// The batches, in reverse, so that handing one out is a pop rather than a shift.
    batches: Vec<RecordBatch>,
}

impl Iterator for Decoded {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.batches.pop().map(Ok)
    }
}

impl RecordBatchReader for Decoded {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

/// The version of iris this library was built from, as a nul terminated string.
///
/// Valid for the life of the process and not to be freed.
#[unsafe(no_mangle)]
pub extern "C" fn iris_version() -> *const c_char {
    // A nul is appended here rather than at every call, so the pointer can be handed straight out.
    const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr().cast::<c_char>()
}

/// Makes a runtime.
///
/// Returns null if the engine could not be built, which in practice means Wasmtime refused the
/// configuration this library asks for and there is nothing a caller can do about it.
#[unsafe(no_mangle)]
pub extern "C" fn iris_runtime_new() -> *mut IrisRuntime {
    match Runtime::new() {
        Ok(runtime) => Box::into_raw(Box::new(IrisRuntime {
            runtime: Arc::new(runtime),
        })),
        Err(_) => ptr::null_mut(),
    }
}

/// Releases a runtime handle. Null is accepted and does nothing.
///
/// # Safety
///
/// `runtime` must be a pointer from [`iris_runtime_new`] that has not already been passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_runtime_free(runtime: *mut IrisRuntime) {
    if runtime.is_null() {
        return;
    }
    // SAFETY: the caller promises this came from `iris_runtime_new` and has not been freed, so it is
    // a pointer from `Box::into_raw` on a live allocation and taking the box back is the pairing.
    drop(unsafe { Box::from_raw(runtime) });
}

/// Names a directory to keep compiled decoders in.
///
/// Off until this is called. The directory holds machine code, so one that another user can write
/// into is one that can hand this process anything, and picking it is an operator's decision the
/// same way allowing a decoder from outside a container is.
///
/// It takes effect for datasets opened afterwards. The builder it stands for takes a runtime by
/// value and a dataset may be holding this one, so what happens here is that the handle is pointed
/// at a new runtime rather than the old one being changed. Call it before opening anything, which
/// is the only order in which the sentence above is not something a caller has to think about.
///
/// # Safety
///
/// `runtime` must be a live pointer from [`iris_runtime_new`], `dir` a nul terminated string, and
/// `error` null or one writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_runtime_set_compilation_cache(
    runtime: *mut IrisRuntime,
    dir: *const c_char,
    error: *mut *mut c_char,
) -> i32 {
    if runtime.is_null() || dir.is_null() {
        return IRIS_INVALID;
    }
    // SAFETY: the caller promises a live handle, and this borrow ends before the function returns.
    let handle = unsafe { &mut *runtime };
    // SAFETY: the caller promises a nul terminated string that outlives this call.
    let Ok(dir) = unsafe { CStr::from_ptr(dir) }.to_str() else {
        return IRIS_INVALID;
    };

    match Runtime::new() {
        Ok(runtime) => {
            handle.runtime = Arc::new(runtime.with_compilation_cache(dir));
            IRIS_OK
        }
        // SAFETY: `error` is null or one writable pointer.
        Err(err) => unsafe { report(error, &err.to_string()) },
    }
}

/// Opens a container held in memory.
///
/// The bytes are copied, so the caller may release them as soon as this returns.
///
/// # Safety
///
/// `runtime` must be a live pointer from [`iris_runtime_new`], `bytes` must point at `len` readable
/// bytes, `out` must point at one writable pointer, and `error` must be null or point at one
/// writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_open(
    runtime: *const IrisRuntime,
    bytes: *const u8,
    len: usize,
    out: *mut *mut IrisDataset,
    error: *mut *mut c_char,
) -> i32 {
    if runtime.is_null() || bytes.is_null() || out.is_null() {
        return IRIS_INVALID;
    }
    // SAFETY: the caller promises a live handle for the duration of the call.
    let handle = unsafe { &*runtime };
    // SAFETY: the caller promises `len` readable bytes at `bytes`, and this borrow does not outlive
    // the copy on the next line.
    let bytes: Box<[u8]> = unsafe { std::slice::from_raw_parts(bytes, len) }.into();

    // SAFETY: `out` is one writable pointer by the contract above, and `error` is checked inside.
    unsafe { finish(Arc::clone(&handle.runtime), bytes, out, error) }
}

/// Opens a container in a file, read whole.
///
/// This is the convenience the example uses and it reads the file into memory. A host that wants the
/// windowed path, where a container larger than memory is read a range at a time, is a host writing
/// Rust today.
///
/// # Safety
///
/// `runtime` must be a live pointer from [`iris_runtime_new`], `path` a nul terminated string, `out`
/// one writable pointer, and `error` null or one writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_open_path(
    runtime: *const IrisRuntime,
    path: *const c_char,
    out: *mut *mut IrisDataset,
    error: *mut *mut c_char,
) -> i32 {
    if runtime.is_null() || path.is_null() || out.is_null() {
        return IRIS_INVALID;
    }
    // SAFETY: the caller promises a live handle for the duration of the call.
    let handle = unsafe { &*runtime };
    // SAFETY: the caller promises a nul terminated string that outlives this call.
    let Ok(path) = unsafe { CStr::from_ptr(path) }.to_str() else {
        return IRIS_INVALID;
    };

    let bytes = match std::fs::read(Path::new(path)) {
        Ok(bytes) => bytes.into_boxed_slice(),
        Err(err) => {
            // SAFETY: `error` is null or one writable pointer, which is what `report` needs.
            return unsafe { report(error, &format!("{path}: {err}")) };
        }
    };

    // SAFETY: `out` is one writable pointer by the contract above, and `error` is checked inside.
    unsafe { finish(Arc::clone(&handle.runtime), bytes, out, error) }
}

/// Releases a dataset handle. Null is accepted and does nothing.
///
/// # Safety
///
/// `dataset` must be a pointer from [`iris_open`] or [`iris_open_path`] that has not already been
/// passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_dataset_free(dataset: *mut IrisDataset) {
    if dataset.is_null() {
        return;
    }
    // SAFETY: the caller promises this came from an open that succeeded and has not been freed, so
    // it is a pointer from `Box::into_raw` on a live allocation.
    drop(unsafe { Box::from_raw(dataset) });
}

/// The name the container carries.
///
/// Points into the dataset and stays valid until it is freed. Never null for a live handle, and null
/// for a null one.
///
/// # Safety
///
/// `dataset` must be a live pointer from an open that succeeded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_dataset_name(dataset: *const IrisDataset) -> *const c_char {
    if dataset.is_null() {
        return ptr::null();
    }
    // SAFETY: the caller promises a live handle, and the pointer returned points into it, which is
    // what the documentation above tells the caller.
    unsafe { &*dataset }.name.as_ptr()
}

/// Fills in an `ArrowSchema` the caller owns.
///
/// The caller releases it by calling its `release` member, which is the Arrow C data interface rule
/// rather than one of this library's.
///
/// # Safety
///
/// `dataset` must be a live pointer from an open that succeeded, `out` must point at one writable
/// `ArrowSchema`, initialised or not, and `error` must be null or one writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_dataset_schema(
    dataset: *const IrisDataset,
    out: *mut FFI_ArrowSchema,
    error: *mut *mut c_char,
) -> i32 {
    if dataset.is_null() || out.is_null() {
        return IRIS_INVALID;
    }
    // SAFETY: the caller promises a live handle for the duration of the call.
    let dataset = unsafe { &*dataset };

    match FFI_ArrowSchema::try_from(dataset.schema.as_ref()) {
        Ok(schema) => {
            // SAFETY: the caller promises one writable `ArrowSchema` at `out`. Writing rather than
            // assigning is right even when it holds a live schema already, because the interface
            // says a consumer passes an uninitialised structure and dropping what might be there
            // would be running a release callback this library did not install.
            unsafe { ptr::write(out, schema) };
            IRIS_OK
        }
        // SAFETY: `error` is null or one writable pointer.
        Err(err) => unsafe { report(error, &err.to_string()) },
    }
}

/// Scans every row of every column and fills in an `ArrowArrayStream` the caller owns.
///
/// The scan happens inside this call. What the caller gets back is a stream over batches that are
/// already decoded, so nothing it does with the stream can fail for a reason that has to do with
/// iris.
///
/// # Safety
///
/// `dataset` must be a live pointer from an open that succeeded, `out` must point at one writable
/// `ArrowArrayStream`, initialised or not, and `error` must be null or one writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_dataset_scan(
    dataset: *const IrisDataset,
    out: *mut FFI_ArrowArrayStream,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: every promise the caller made is passed straight through, and `columns` being null
    // with a count of zero is the whole table, which is what this function is.
    unsafe { iris_dataset_scan_columns(dataset, ptr::null(), 0, out, error) }
}

/// Scans the named columns, by position in the schema.
///
/// A decoder that agreed to projection is told which columns to read and fetches the bytes of those
/// and no others. One that did not has every column read and the wanted ones taken out of the
/// batches afterwards. Both give the same answer and only one of them moves fewer bytes.
///
/// A `count` of zero reads every column, whatever `columns` is.
///
/// # Safety
///
/// `dataset` must be a live pointer from an open that succeeded, `columns` must point at `count`
/// readable `uint32_t` unless `count` is zero, `out` must point at one writable `ArrowArrayStream`,
/// initialised or not, and `error` must be null or one writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_dataset_scan_columns(
    dataset: *const IrisDataset,
    columns: *const u32,
    count: usize,
    out: *mut FFI_ArrowArrayStream,
    error: *mut *mut c_char,
) -> i32 {
    if dataset.is_null() || out.is_null() || (columns.is_null() && count != 0) {
        return IRIS_INVALID;
    }
    // SAFETY: the caller promises a live handle for the duration of the call.
    let dataset = unsafe { &*dataset };

    let opened = match dataset.runtime.open(&dataset.bytes) {
        Ok(opened) => opened,
        // SAFETY: `error` is null or one writable pointer.
        Err(err) => return unsafe { report(error, &err.to_string()) },
    };

    let scanned = if count == 0 {
        opened.scan()
    } else {
        // SAFETY: the caller promises `count` readable `u32` at `columns`, and this borrow ends
        // inside the call below.
        opened.scan_columns(unsafe { std::slice::from_raw_parts(columns, count) })
    };

    let mut batches = match scanned {
        Ok(batches) => batches,
        // SAFETY: `error` is null or one writable pointer.
        Err(err) => return unsafe { report(error, &err.to_string()) },
    };

    // The schema of the batches rather than the schema of the container, because a projection means
    // the two are different and the stream has to describe what is in it.
    let schema = batches
        .first()
        .map_or_else(|| Arc::clone(&dataset.schema), RecordBatch::schema);
    batches.reverse();

    let stream = FFI_ArrowArrayStream::new(Box::new(Decoded { schema, batches }));
    // SAFETY: the caller promises one writable `ArrowArrayStream` at `out`, and the reasoning about
    // writing rather than assigning is the one at `iris_dataset_schema`.
    unsafe { ptr::write(out, stream) };
    IRIS_OK
}

/// Releases a message this library wrote to an error argument. Null is accepted and does nothing.
///
/// # Safety
///
/// `message` must be a pointer this library wrote to an `error` argument and has not already been
/// passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iris_string_free(message: *mut c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: the caller promises this came from `report`, which produces it with
    // `CString::into_raw`, and that it has not been freed.
    drop(unsafe { CString::from_raw(message) });
}

/// Opens the bytes, keeps what the accessors return, and writes the handle out.
///
/// # Safety
///
/// `out` must point at one writable pointer and `error` must be null or point at one.
unsafe fn finish(
    runtime: Arc<Runtime>,
    bytes: Box<[u8]>,
    out: *mut *mut IrisDataset,
    error: *mut *mut c_char,
) -> i32 {
    let (name, schema) = match runtime.open(&bytes) {
        Ok(opened) => match CString::new(opened.name()) {
            Ok(name) => (name, Arc::clone(opened.schema())),
            // A nul inside a name is not something a container should carry and there is no way to
            // hand it to a C caller, so it is refused here rather than truncated silently.
            Err(_) => {
                // SAFETY: `error` is null or one writable pointer.
                return unsafe { report(error, "the name in this container contains a nul byte") };
            }
        },
        // SAFETY: `error` is null or one writable pointer.
        Err(err) => return unsafe { report(error, &err.to_string()) },
    };

    let dataset = Box::into_raw(Box::new(IrisDataset {
        runtime,
        bytes,
        name,
        schema,
    }));
    // SAFETY: the caller promises one writable pointer at `out`.
    unsafe { ptr::write(out, dataset) };
    IRIS_OK
}

/// Writes a message to an error argument if the caller asked for one, and returns [`IRIS_ERROR`].
///
/// A message with a nul in it cannot be handed over, and losing the status because of that would be
/// worse than losing the text, so the status comes back either way.
///
/// # Safety
///
/// `error` must be null or point at one writable pointer.
unsafe fn report(error: *mut *mut c_char, message: &str) -> i32 {
    if !error.is_null()
        && let Ok(message) = CString::new(message)
    {
        // SAFETY: the caller promises one writable pointer at `error`.
        unsafe { ptr::write(error, message.into_raw()) };
    }
    IRIS_ERROR
}
