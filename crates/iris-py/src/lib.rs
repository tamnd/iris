//! The Python bindings for iris.
//!
//! A wrapper over the C ABI in `iris-c`, calling its safe side rather than its entry points. That is
//! the whole design: Python and C get the same open, the same projection rule and the same reopen
//! behaviour because there is one implementation of those and this file does not contain a second
//! one. What it adds is the Python end of the Arrow contract.
//!
//! # How the data gets out
//!
//! Through the Arrow `PyCapsule` interface, which is the Python spelling of the same C data interface
//! the C ABI hands back. A dataset has `__arrow_c_schema__` and `__arrow_c_stream__`, so
//! `pyarrow.table(dataset)` works, and so does anything else that speaks the protocol: polars,
//! `DuckDB`, nanoarrow. Nothing here imports pyarrow and the wheel does not depend on it.
//!
//! The buffers are not copied. What crosses is an `ArrowArrayStream` in a capsule, the consumer
//! takes ownership of the batches by moving the structure out of it, and the memory those batches
//! point at is the memory the decoder wrote. `tests/test_iris.py` checks that by watching pyarrow's
//! allocator, which does not grow when a table is imported this way and would if it were a copy.
//!
//! # Why there is no free
//!
//! A C caller frees a runtime and a dataset by hand. Here they are Python objects and the
//! interpreter frees them, which is the one part of the C ABI that does not carry over and the one
//! part nobody wants carried over.

use std::ffi::CStr;
use std::path::PathBuf;
use std::sync::Arc;

// The C ABI crate is `iris-c` and the library it builds is `iris`, because a C programmer writes
// `-liris`. This is that library.
use iris::{Error, IrisDataset, IrisRuntime};
use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCapsule};

/// A runtime, which is where compiled decoders are held.
///
/// One per process is the point of it. Every dataset opened from the same runtime shares its
/// decoder pool, so the second container to be opened finds its decoder already compiled. Making a
/// runtime per file throws that away and is the one mistake this API makes easy.
#[pyclass(name = "Runtime", module = "iris")]
struct PyRuntime {
    /// The handle from the C ABI, used through its safe methods.
    inner: IrisRuntime,
}

#[pymethods]
impl PyRuntime {
    #[new]
    fn new() -> PyResult<Self> {
        IrisRuntime::new()
            .map(|inner| Self { inner })
            .map_err(|err| raised(&err))
    }

    /// Names a directory to keep compiled decoders in. Off until this is called.
    ///
    /// The directory holds machine code that this process will map executable, so one that another
    /// user can write into is one that can hand this process anything. iris does not pick a
    /// location, does not have a default one, and does not check who owns the one it is given.
    ///
    /// It takes effect for datasets opened afterwards, so call it before opening anything.
    fn set_compilation_cache(&mut self, directory: &str) -> PyResult<()> {
        self.inner
            .set_compilation_cache(directory)
            .map_err(|err| raised(&err))
    }

    /// Opens a container held in memory. The bytes are copied and may be released afterwards.
    fn open(&self, data: &Bound<'_, PyBytes>) -> PyResult<PyDataset> {
        opened(self.inner.open(data.as_bytes().into()))
    }

    /// Opens a container in a file, read whole.
    ///
    /// A container larger than memory is read a range at a time through the windowed path, and that
    /// path is Rust only today.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "pyo3 builds the argument out of the Python object, so it arrives owned"
    )]
    fn open_path(&self, path: PathBuf) -> PyResult<PyDataset> {
        opened(self.inner.open_path(&path))
    }

    #[expect(
        clippy::unused_self,
        reason = "a Python method takes self whether or not it reads anything out of it"
    )]
    fn __repr__(&self) -> &'static str {
        "<iris.Runtime>"
    }
}

/// An open container.
///
/// It is an Arrow stream source, so `pyarrow.table(dataset)` reads the whole thing and
/// `pyarrow.schema(dataset)` asks what is in it without reading anything.
#[pyclass(name = "Dataset", module = "iris", frozen)]
struct PyDataset {
    /// Shared with every scan taken from it, so a scan handed to a consumer keeps the container
    /// alive whatever Python does with the dataset afterwards.
    inner: Arc<IrisDataset>,
}

#[pymethods]
impl PyDataset {
    /// The name the container carries.
    #[getter]
    fn name(&self) -> PyResult<&str> {
        self.inner
            .name()
            .to_str()
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    /// The number of columns in the container's schema.
    #[getter]
    fn num_columns(&self) -> usize {
        self.inner.schema().fields().len()
    }

    /// The names of those columns, in schema order, which is the order a projection counts in.
    #[getter]
    fn column_names(&self) -> Vec<String> {
        self.inner
            .schema()
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect()
    }

    /// A scan of some of the columns, by position in the schema.
    ///
    /// A decoder that agreed to projection is told which columns to read and fetches the bytes of
    /// those and no others. One that did not has every column read and the wanted ones taken out of
    /// the batches afterwards. Both give the same answer and only one of them moves fewer bytes.
    ///
    /// `None`, which is the default, reads every column.
    #[pyo3(signature = (columns=None))]
    fn scan(&self, columns: Option<Vec<u32>>) -> PyScan {
        PyScan {
            dataset: Arc::clone(&self.inner),
            columns: columns.unwrap_or_default(),
        }
    }

    /// The schema, as an Arrow C data interface capsule.
    fn __arrow_c_schema__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyCapsule>> {
        capsule(
            py,
            self.inner.ffi_schema().map_err(|err| raised(&err))?,
            c"arrow_schema",
        )
    }

    /// Every column, as an Arrow C data interface capsule. The scan happens in this call.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(
        &self,
        py: Python<'py>,
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyCapsule>> {
        self.scan(None).__arrow_c_stream__(py, requested_schema)
    }

    fn __repr__(&self) -> PyResult<String> {
        Ok(format!(
            "<iris.Dataset {:?}, {} columns>",
            self.name()?,
            self.num_columns()
        ))
    }
}

/// A scan of a dataset that has not happened yet.
///
/// It exists so that a projection can be handed to a consumer as one object, since the Arrow
/// protocol asks for `__arrow_c_stream__` on the thing being consumed and not for an argument.
#[pyclass(name = "Scan", module = "iris", frozen)]
struct PyScan {
    /// The container this reads.
    dataset: Arc<IrisDataset>,
    /// Column positions, empty for every column.
    columns: Vec<u32>,
}

#[pymethods]
impl PyScan {
    /// The batches, as an Arrow C data interface capsule.
    ///
    /// The scan happens in this call, so what the consumer gets is a stream over batches that are
    /// already decoded and nothing it does with the stream can fail for a reason to do with iris.
    ///
    /// `requested_schema` is refused rather than ignored. A decoder reads what the container says
    /// it holds, casting afterwards is something the consumer can do better, and a producer that
    /// quietly hands back a different schema from the one asked for is worse than one that says it
    /// cannot.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(
        &self,
        py: Python<'py>,
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyCapsule>> {
        if requested_schema.is_some_and(|schema| !schema.is_none()) {
            return Err(PyNotImplementedError::new_err(
                "iris reads the schema the container carries and cannot produce another one",
            ));
        }

        let stream = py
            .detach(|| self.dataset.stream(&self.columns))
            .map_err(|err| raised(&err))?;
        capsule(py, stream, c"arrow_array_stream")
    }

    fn __repr__(&self) -> String {
        if self.columns.is_empty() {
            "<iris.Scan, every column>".to_owned()
        } else {
            format!("<iris.Scan, columns {:?}>", self.columns)
        }
    }
}

/// Puts an Arrow C structure in a capsule under the name the protocol gives it.
///
/// The capsule owns the structure and releasing it is what the Arrow rules say: a consumer that
/// takes the contents leaves the source marked released, and one that never looks lets the drop
/// here call the release callback.
fn capsule<'py, T: Send + 'static>(
    py: Python<'py>,
    value: T,
    name: &'static CStr,
) -> PyResult<Bound<'py, PyCapsule>> {
    PyCapsule::new_with_value(py, value, name)
}

/// Turns an error from the C ABI into the exception Python sees.
///
/// One exception type, because the distinctions the `Error` enum makes are about where the failure
/// came from rather than about what a caller would do differently, and inventing a hierarchy that
/// nobody catches selectively is not an improvement.
fn raised(err: &Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

/// Wraps what an open produced, so the two open methods differ only in what they are given.
fn opened(result: Result<IrisDataset, Error>) -> PyResult<PyDataset> {
    result
        .map(|dataset| PyDataset {
            inner: Arc::new(dataset),
        })
        .map_err(|err| raised(&err))
}

#[pymodule]
#[pyo3(name = "_iris")]
fn bindings(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add_class::<PyRuntime>()?;
    module.add_class::<PyDataset>()?;
    module.add_class::<PyScan>()?;
    Ok(())
}
