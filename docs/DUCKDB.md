# Reading an iris container from DuckDB

```sql
load 'iris.duckdb_extension';
select * from iris_scan('sample.iris');
```

That is the whole thing. DuckDB has no reader for the iris format and this extension does not add one. The container carries its own decoder, so the extension is the part that opens the file, asks for the decoder, and hands the batches over.

The extension is unsigned, so a session has to be started with `duckdb -unsigned` or opened with `allow_unsigned_extensions` set. DuckDB signs its own extensions and refuses everybody else's by default, which is the right default and is not something to work around anywhere but on the command line that loads this one.

Every release attaches one per platform, as `iris-<version>-<platform>.duckdb_extension.zip`. Unzip it and what comes out is a file called `iris.duckdb_extension`, which has to keep that name. DuckDB reads the name of an extension off the file name and then looks for an entry point called after it, so a copy renamed to say which platform it is for loads and then fails to find the function it came for, with a message about a missing symbol. That is why the platform is in the name of the zip and not in the name of the file.

## The whole API

```sql
select * from iris_scan('sample.iris');
select c0, c2 from iris_scan('sample.iris') where c0 > 500;
select count(*) from iris_scan('sample.iris');
describe select * from iris_scan('sample.iris');
```

One table function taking one path. The columns are whatever the container says it holds, so `describe` answers out of the file rather than out of anything the extension knows.

`IRIS_COMPILATION_CACHE` in the environment names a directory to keep compiled decoders in, which is the same cache `docs/PYTHON.md` and `docs/C_ABI.md` describe and has the same warning on it: an entry in that directory is machine code this process will map executable.

## Projection reaches the decoder

DuckDB says which columns a query wants, and those are the columns the container is asked for. A scan of two columns out of four is the decoder reading two, not the extension reading four and discarding half. A decoder that did not agree to projection has every column read and the wanted ones taken out afterwards, which gives the same answer and moves more bytes, and which of the two happened is a property of the decoder in the file rather than of this extension.

A `count(*)` asks for no columns at all. That is its own path: one column is read to find out how many rows there are and none of it is written into the result. Asking iris for no columns means asking for all of them, so the obvious version of this reads the whole container to answer a count.

## Two Arrows

The `duckdb` crate this is built with is on Arrow 58 and the rest of the iris workspace is on Arrow 59. To the compiler, the batches iris produces are not the batches DuckDB accepts.

Crossing that is two functions of one line each. What moves between the two halves is an `ArrowSchema` and an `ArrowArrayStream` from the Arrow C data interface, and those are C structures whose layout the specification fixes rather than Rust types whose layout a compiler picks. Both crates declare them `#[repr(C)]` with the same fields in the same order, so a value of one is a value of the other and moving it is a move.

This is worth pausing on, because it is the argument iris makes about file formats turning up uninvited in a dependency graph. A major version skew between two libraries that have to exchange data is normally a fork, a vendored copy, or a wait for an upstream release. Here it costs nothing, and the reason it costs nothing is that somebody specified the boundary as a byte layout instead of as a set of types. That is the same reason a container with a decoder in it can be read by an engine that has never heard of it.

## How big it is

The extension is 275 lines, of which 146 are code and the rest are comments and blank lines. The build script, the metadata writer and the session test that checks the answers bring everything this integration is made of to 552 lines.

That number is the point rather than a detail. BtrBlocks reached DuckDB in 586 lines and three person days, and that is the strongest single piece of evidence anybody has that a self describing format is cheap for an engine to adopt. This is the same claim made once more with the decoder inside the file rather than inside the extension, and it comes out about the same size. An engine that wanted to read iris would be doing roughly this much work.

## Building it

```
ci/duckdb-extension.sh
ci/duckdb-session.sh
```

The first builds the shared library and appends the metadata block DuckDB reads before it will load anything. It needs no DuckDB: the crate is built against the C extension API rather than against a database, so it names no DuckDB symbol and reaches every function it calls through the table of pointers DuckDB hands it at load. That is what lets a release build one of these for five platforms on machines that have none of them installed.

The second is the test. It writes a container, builds the extension, starts a real `duckdb`, and checks five answers: the shape of the table, every column, two columns of four, a count, and one row read whole. The crate has no cargo test target because the functions it calls do not exist until a database loads it, so a database process reading a file is the only honest test there is.

`--target <triple>` cross builds. The five platforms are `osx_arm64`, `osx_amd64`, `linux_amd64`, `linux_arm64` and `windows_amd64`, which are DuckDB's names for them, and the mapping from a Rust target triple is in `ci/duckdb-extension.sh`.

## The metadata block

A `.duckdb_extension` file is a shared library with 534 bytes on the end of it. DuckDB will not load one without them, and the error when they are missing says the file is not a valid extension rather than saying what about it is invalid, so it is worth knowing what is in there.

The block is a WebAssembly custom section header naming a section called `duckdb_signature`, then eight fields of 32 bytes each written from the last to the first, then 256 bytes of signature that are zero for anything nobody signed. The fields that matter are the ABI type, which is `C_STRUCT` here, the version of the extension, the C API version it asked for, and the platform. `ci/duckdb-metadata.py` writes it and says what each field is for.

The platform field is the one that bites. It is compared against the platform of the session before anything is loaded, so an extension built for `linux_amd64` and loaded on `osx_arm64` is refused with a message about the platform and not about anything that is actually wrong with the file. `ci/duckdb-session.sh` reads that field back out and checks it against `pragma platform` before it runs a query, because that failure is worth catching where it can be explained.

## What is not here

No writer. This reads containers and there is no `copy to` that produces one.

No pushdown of filters, and no reason to add one yet. A decoder is told which columns to read and not which rows, so a `where` clause is DuckDB's work and DuckDB is good at it.

A container larger than memory is read a range at a time through the windowed path in `iris-runtime`, and this extension does not use it. From DuckDB, a container is opened whole.

## Another way in

If there is a Python interpreter about, none of this is needed:

```python
import iris, duckdb
dataset = iris.Runtime().open_path("sample.iris")
duckdb.sql("select count(*) from dataset")
```

DuckDB consumes the Arrow PyCapsule interface and `iris-py` produces it, so that has worked since the Python bindings existed and no extension is involved. What the extension is for is the session that has no Python in it, which is most of them.
