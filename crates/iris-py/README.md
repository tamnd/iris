# iris

Ship the decoder with the data.

A container carries a reference to a WebAssembly decoder alongside its bytes, and this reads one without linking a format specific reader. Open it and hand it to anything that speaks Arrow.

```python
import iris
import pyarrow as pa

runtime = iris.Runtime()
dataset = runtime.open_path("sample.iris")
table = pa.table(dataset)
```

`pa.table` works because a dataset carries `__arrow_c_stream__`, which is the Arrow PyCapsule interface, so polars, DuckDB and nanoarrow read one the same way. Nothing in the wheel imports pyarrow and the wheel does not depend on it. That is the reason there is no `to_pandas` and no `to_table` here: the protocol already exists, every consumer worth handing bytes to speaks it, and a tenth version of it is not an improvement.

The buffers are not copied on the way across. What the consumer takes ownership of is the memory the decoder wrote, and `tests/test_iris.py` checks that by watching pyarrow's allocator, which does not grow when a table is imported this way.

## The whole API

```python
runtime = iris.Runtime()
runtime.set_compilation_cache("/var/cache/iris/decoders")   # off until this is called

dataset = runtime.open_path("sample.iris")
dataset = runtime.open(open("sample.iris", "rb").read())

dataset.name, dataset.num_columns, dataset.column_names
pa.schema(dataset)
pa.table(dataset)
pa.table(dataset.scan([0, 2]))                              # by position in the schema
```

One runtime per process is the point of the runtime. Every dataset opened from the same one shares its compiled decoders, so the second container to be opened does not compile anything. Making a runtime per file throws that away, and with a compilation cache named it is the difference between tens of milliseconds and a fraction of one. `docs/COLD_START.md` in the repository has the numbers.

## The name

The distribution is `irisdb` on PyPI and the module is `iris`, for the same reason the crate is `irisdb` on crates.io and the command line tool it installs is called `iris`: the bare name was taken in both places before this project existed.

## What it is not

This is a wrapper over the C ABI in `iris-c` rather than a second implementation. Python and C get the same open, the same projection rule and the same behaviour on a container that reopens for each scan, because there is one implementation of all three and neither binding contains a copy of it.

A container larger than memory is read a range at a time through the windowed path, and that path is Rust only today. From Python, a container is opened whole.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
