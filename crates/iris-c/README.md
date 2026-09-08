# iris-c

The C ABI for iris.

A shared library, a static library and one header. Everything with structure in it comes back as an Arrow C structure, so the header is five handles and ten functions and contains no description of what a column is. Anything that already reads Arrow reads what this produces with no glue at all, and anything that does not is not a consumer this ABI should be designing for.

`docs/C_ABI.md` is the guide, `include/iris.h` is the header, and `examples/scan.c` is a complete program that opens a container and prints what is in it. The library is called `iris` rather than `iris_c`, so a C build writes `-liris`, and the package keeps the longer name because that is a cargo concern rather than a C one.

Errors are handed back with the call that produced them. Every fallible entry point returns a status and takes a `char **error` last, and the message written there belongs to the caller. The usual shape for this is a `iris_last_error` reading a thread local, and there is not one, because `ci/discipline.py` refuses thread locals anywhere in this tree and because a per thread slot is the wrong answer the moment two threads open two containers.

A dataset holds the bytes it was opened over and opens the container again for each scan. A borrow is not a thing that crosses a C boundary, and the alternative is a struct that points into itself, sound only by an argument nobody reviewing a file like this should have to check. Reopening costs about four hundredths of a millisecond against the decoder pool, which `docs/COLD_START.md` measures, and the name and the schema are read once and kept.

Under the `extern "C"` layer there is a safe Rust one, on `IrisRuntime` and `IrisDataset`, and the entry points are adapters over it that check pointers and turn an error into a message. That split is there because `iris-py` is a wrapper over this ABI rather than a second implementation of it, and a wrapper written against raw pointers would be ceremony with unsafe in it. Python and C get the same open, the same projection rule and the same reopen behaviour because there is one implementation of all three.

Not on crates.io. What this crate produces is a shared library, a static library and a header, none of which is a thing cargo installs, so they come out of the release rather than out of the registry.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
