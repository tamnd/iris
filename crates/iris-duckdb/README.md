# iris-duckdb

The DuckDB extension for iris.

One table function. `select * from iris_scan('c.iris')` opens a container, compiles its decoder if this process has not seen it before, and hands the batches to DuckDB. There is no reader for the iris format in DuckDB and this crate does not add one, which is the point of the exercise: the container carries its own decoder, so what an engine needs is not a reader but a way to ask for one.

`docs/DUCKDB.md` is the guide.

The whole crate is 275 lines, of which 146 are code and the rest are comments and blank lines. With the build script, the metadata writer and the session test that checks the answers, everything this integration is made of comes to 552 lines. The number is here because it is the argument. BtrBlocks reached DuckDB in 586 lines and three person days, and that is the strongest single piece of evidence that a self describing format is cheap for an engine to adopt. This is the same claim with the decoder inside the file instead of inside the extension.

Nothing here links DuckDB. The `loadable-extension` feature reaches every DuckDB function through the table of pointers DuckDB hands an extension when it loads it, so what gets built is a shared library that names no DuckDB symbol and can be built on a machine that has no DuckDB on it. `ci/duckdb-extension.sh` builds it and appends the metadata block, and `ci/duckdb-session.sh` runs a real session against a real container and checks the answers, which is the only honest test for a library whose callees do not exist until something loads it.

The `duckdb` crate is built against Arrow 58 and the rest of this workspace is on Arrow 59, so to the compiler the batches iris produces are not the batches DuckDB accepts. Crossing that is two functions of one line each, because what moves between them is an `ArrowSchema` and an `ArrowArrayStream` from the Arrow C data interface, and those are C structures whose layout the specification fixes rather than Rust types whose layout a compiler picks. It is worth noticing that this is the argument iris makes about file formats, arriving unasked for in a dependency graph: a version skew that usually means forking a crate or waiting for a release costs nothing, because the boundary was specified as a byte layout.

Projection reaches the decoder. DuckDB says which columns a query wants and those are the columns the container is asked for, so a scan of two columns out of four is the decoder reading two. A `count(*)` asks for no columns at all, which is its own path: one column is read to find out how many rows there are and none of it is written out.

Not on crates.io. What this crate produces is a `.duckdb_extension` file, which is a shared library with a trailer on it and not a thing cargo installs, so it comes out of the release.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
