# Reading an iris container from C

Download the archive for your platform from a release, unpack it, and compile against the header in it. Nothing else is needed and nothing is assumed: there is no Rust in the archive and none has to be on the machine.

```
cc -I include examples/scan.c -L lib -liris -o scan
./scan sample.iris
```

That prints the name of the container, its columns, and the number of rows the scan produced. `sample.iris` is in the archive so that the first thing anybody tries has something to open.

## What is in the archive

| Path | What it is |
| --- | --- |
| `include/iris.h` | The header. Self contained, including the Arrow C data interface structures under the guards the Arrow specification asks for. |
| `lib/libiris.so`, `lib/libiris.dylib`, `lib/iris.dll` | The shared library, one of the three depending on the platform. |
| `lib/libiris.a`, `lib/iris.lib` | The static library, for a program that would rather not ship a second file. |
| `examples/scan.c` | The program above, complete. |
| `sample.iris` | A container of a thousand rows and four columns, with its decoder inside it. |
| `iris`, `iris.exe` | The command line tool, which is a separate thing that happens to travel in the same archive. |

The shared library has to be findable at run time. The quickest way is an environment variable, which is `LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS, and the directory of the executable or `PATH` on Windows. The way that survives being installed is to record the location in the program:

```
cc -I include examples/scan.c -L lib -liris -Wl,-rpath,'$ORIGIN/lib' -o scan      # Linux
cc -I include examples/scan.c -L lib -liris -Wl,-rpath,@loader_path/lib -o scan   # macOS
```

The macOS library is stamped with `@rpath/libiris.dylib`, which is what makes the second line work. Linking the static library instead avoids the question and costs about a hundred megabytes, most of which is the WebAssembly compiler.

## The shape of the API

Ten functions. Five give out handles or release them, four ask a dataset something, and one frees a message.

```c
IrisRuntime *runtime = iris_runtime_new();

char *error = NULL;
IrisDataset *dataset = NULL;
if (iris_open_path(runtime, "sample.iris", &dataset, &error) != IRIS_OK) {
  fprintf(stderr, "%s\n", error);
  iris_string_free(error);
  return 1;
}

struct ArrowArrayStream stream;
iris_dataset_scan(dataset, &stream, &error);
```

A runtime is where compiled decoders are held and one is enough for a process. A dataset is an open container. Both may be used from several threads at once, and neither is pinned to the thread it was made on.

## Errors

Every call that can fail returns an `int32_t` and takes a `char **error` as its last argument.

`IRIS_OK` means it worked and nothing was written to `error`. `IRIS_ERROR` means it did not and the message is in `error`, owned by the caller and released with `iris_string_free`. `IRIS_INVALID` means an argument the call cannot do without was null, so nothing was attempted and there is no message, because that is a bug in the calling program rather than a condition it should be handling at run time.

Passing `NULL` for `error` is allowed. The status still comes back.

There is no `iris_last_error`. A last error slot is per thread state, this project does not have any, and `ci/discipline.py` fails a build that grows one. The reason is not stylistic: a decode job in iris moves between threads, and state a job carries without owning is state it loses the moment it moves.

## Arrow, and why there is nothing else

A schema comes back as an `ArrowSchema` and a scan comes back as an `ArrowArrayStream`. Both are the Arrow C data interface, which is a stable C level contract that pandas, DuckDB, Polars, nanoarrow, the Arrow C++ and Go and Rust libraries and a long tail of others already speak.

So the alternative was inventing a column representation, a null representation, a string representation and a way to hand them over, all of which exist, are agreed on, and are not improved by a tenth version. What that decision costs is that a caller who wants the values of a column needs an Arrow library to read them with. What it buys is that the same caller needs no iris specific code at all beyond the ten functions above.

The structures are declared in `iris.h` under `ARROW_C_DATA_INTERFACE` and `ARROW_C_STREAM_INTERFACE`, which is the guard the specification tells everyone to use, so including this header next to another one that also carries them is fine.

Releasing them is the Arrow rule rather than an iris rule: call the `release` member on the structure. A stream is drained by calling `get_next` until it hands back an array whose `release` is null.

## What a scan actually does

The scan happens inside `iris_dataset_scan`. What comes back is a stream over batches that have already been decoded, so nothing done with the stream afterwards can fail for a reason that has to do with iris, and `get_last_error` is there because the interface has it rather than because it has anything to say.

That is worth knowing rather than inferring from the word stream. A container larger than memory is read a range at a time through the windowed path, and that path is Rust only today. From C, a container is opened whole.

`iris_dataset_scan_columns` takes column positions in the schema. A decoder that agreed to projection is told which columns to read and fetches the bytes of those and no others. One that did not has every column read and the wanted ones taken out of the batches afterwards. Both give the same answer and only one of them moves fewer bytes. A count of zero means every column.

## Keeping compiled decoders

```c
iris_runtime_set_compilation_cache(runtime, "/var/cache/iris/decoders", &error);
```

Off until this is called. With it, the second process to open a container reads the machine code its decoder compiles to instead of compiling it again, which takes an open from tens of milliseconds to a fraction of one. `docs/COLD_START.md` has the numbers.

Call it before opening anything. It applies to datasets opened afterwards, and the reason is in the header.

An entry in that directory is machine code that this process will map executable, so a directory another user can write into is a directory that can hand this process anything. iris does not pick a location, does not have a default one, and does not check who owns the one it is given. Choosing it is an operator's decision of the same kind as allowing a decoder from outside a container.

## Building the sample container yourself

`sample.iris` in the archive was produced by this, which needs Rust and is the only thing on this page that does:

```
cargo run --release -p iris-runtime --example write_container -- sample.iris --rows 1000 --columns 4
```

## How this is checked

`crates/iris-c/tests/abi.rs` calls every entry point the way a C caller does, in process, on all four platforms CI runs on. That covers the argument checking, the ownership rules and the error paths.

What it cannot cover is the thing that actually goes wrong with a native library, which is that it does not link or does not load somewhere. So the `Release` workflow builds the archive, deletes the Rust toolchain off the runner, checks that `cargo` and `rustc` are gone, and then compiles `scan.c` with nothing but the platform's own compiler and runs it against `sample.iris`, on Linux, macOS and Windows. A release that fails that is not published.
