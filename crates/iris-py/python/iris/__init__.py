"""Ship the decoder with the data.

A container carries a reference to a WebAssembly decoder alongside its bytes, and this reads one
without linking a format specific reader. Open it and hand it to anything that speaks Arrow.

    import iris
    import pyarrow as pa

    runtime = iris.Runtime()
    dataset = runtime.open_path("sample.iris")
    table = pa.table(dataset)

`pa.table` works because a dataset carries `__arrow_c_stream__`, which is the Arrow PyCapsule
interface, so polars, DuckDB and nanoarrow read one the same way. Nothing here imports pyarrow and
the wheel does not depend on it.

The buffers are not copied on the way across. What the consumer takes is the memory the decoder
wrote.

One runtime per process is the point of the runtime. Every dataset opened from the same one shares
its compiled decoders, so the second container to be opened does not compile anything. Making a
runtime per file throws that away.
"""

from ._iris import Dataset, Runtime, Scan, __version__

__all__ = ["Dataset", "Runtime", "Scan", "__version__"]
