# Reading an iris container from Python

```
pip install irisdb
```

```python
import iris
import pyarrow as pa

runtime = iris.Runtime()
dataset = runtime.open_path("sample.iris")
table = pa.table(dataset)
```

That is the whole thing. There is no build step, no Rust on the machine, and no iris specific way to get at the values, because what comes back is an Arrow table and pyarrow already knows what to do with one.

The distribution is `irisdb` and the module is `iris`, for the same reason the crate is `irisdb` on crates.io and the command line tool it installs is called `iris`: the bare name was taken in both registries before this project existed. One wheel per platform covers every Python from 3.9 up.

## The whole API

```python
runtime = iris.Runtime()
runtime.set_compilation_cache("/var/cache/iris/decoders")

dataset = runtime.open_path("sample.iris")
dataset = runtime.open(raw_bytes)

dataset.name             # the name the container carries
dataset.num_columns
dataset.column_names     # in schema order, which is the order a projection counts in

pa.schema(dataset)       # asks what is in it without reading anything
pa.table(dataset)        # every column
pa.table(dataset.scan([0, 2]))
```

A runtime is where compiled decoders are held and one is enough for a process. A dataset is an open container. Both may be used from several threads at once and neither is pinned to the thread it was made on.

## Arrow, and why there is nothing else

A dataset carries `__arrow_c_schema__` and `__arrow_c_stream__`, which is the Arrow PyCapsule interface. `pa.table` is one consumer of it. So are polars, DuckDB, nanoarrow, and anything else that has adopted the protocol, and all of them work with no code here that knows they exist:

```python
import polars as pl
frame = pl.from_arrow(dataset)

import duckdb
duckdb.sql("select count(*) from dataset")
```

Nothing in the wheel imports pyarrow and the wheel does not depend on it. That is the point of handing data over through a protocol rather than through a library: the alternative was a `to_pandas`, a `to_table`, a `to_polars` and a decision every time somebody adds a dataframe library.

`requested_schema` is refused rather than ignored. A decoder reads what the container says it holds, casting afterwards is something the consumer does better, and a producer that quietly hands back a different schema from the one that was asked for is worse than one that says it cannot.

## The buffers are not copied

What crosses is an `ArrowArrayStream` in a capsule. The consumer takes ownership of the batches by moving the structure out of it, and the memory those batches point at is the memory the decoder wrote. Nothing is serialised, nothing is re-laid-out, and no bytes are moved on the way.

`crates/iris-py/tests/test_iris.py` checks that rather than asserting it, by watching pyarrow's own allocator. An array pyarrow imported through the C data interface points at memory somebody else allocated, so the pool does not grow. An array pyarrow built by copying comes out of that pool and it does. The test next to it casts a column to a narrower integer, which is a copy by definition, and shows the number moving, so the first test is a measurement rather than a constant.

## Keeping compiled decoders

```python
runtime.set_compilation_cache("/var/cache/iris/decoders")
```

Off until this is called. With it, the second process to open a container reads the machine code its decoder compiles to instead of compiling it again, which takes an open from tens of milliseconds to a fraction of one. `docs/COLD_START.md` has the numbers.

Call it before opening anything. It applies to datasets opened afterwards.

An entry in that directory is machine code that this process will map executable, so a directory another user can write into is a directory that can hand this process anything. iris does not pick a location, does not have a default one, and does not check who owns the one it is given. Choosing it is an operator's decision of the same kind as allowing a decoder from outside a container.

## One runtime, not one per file

Every dataset opened from the same runtime shares its compiled decoders, so the second container to be opened does not compile anything. A runtime per file throws that away, and with a compilation cache named it is the difference between tens of milliseconds and a fraction of one. This is the one mistake the API makes easy and it is worth reading twice.

## What is not here

A container larger than memory is read a range at a time through the windowed path, and that path is Rust only today. From Python, a container is opened whole.

There is no free. A C caller releases a runtime and a dataset by hand and here the interpreter does it, which is the one part of the C ABI that does not carry over and the one part nobody wants carried over.

## Building it yourself

```
pip install maturin
maturin develop --release --manifest-path crates/iris-py/Cargo.toml
python -m pytest crates/iris-py/tests
```

The tests write their own container with `cargo run -p iris-runtime --example write_container`, so a checkout with a Rust toolchain needs nothing else. Given `IRIS_SAMPLE` pointing at a container they use that instead, which is how the same tests run on a machine that has no Rust.

## How this is checked

The bindings are a wrapper over the C ABI in `iris-c`, calling its safe side rather than its entry points, so Python and C get the same open, the same projection rule and the same reopen behaviour from one implementation. There is no second copy of any of it to keep in step.

What that leaves is the thing that actually goes wrong with a wheel, which is that it does not install or does not import somewhere. So the `Release` workflow builds a wheel for each of five targets, deletes the Rust toolchain off the runner, checks that `cargo` and `rustc` are gone, installs the wheel from the file with the index turned off, and runs the tests on this page against a container built elsewhere, on Linux, macOS and Windows. A release that fails that is not published, and what goes to PyPI afterwards is downloaded from that release rather than built again.
