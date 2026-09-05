# iris-source

Range oriented data sources for iris.

A decoder declares the byte ranges it needs and the host serves them. That inversion is what lets the same decoder run against a local file, a page cache, and an object store.

`Window` is the first piece of it. It reserves a fixed span of address space once, maps part of a file into it, and moves that part when a request falls outside it, so the address space a dataset costs is chosen by the host rather than by the size of the data. When the view moves, every address the old view covered stops being readable rather than returning bytes from the part of the file that used to be there, which is the property the whole type exists for: a stale read produces an answer that is wrong and looks right, and nothing downstream can catch it.

This is the only crate in the workspace that contains unsafe code. Every other one carries `#![forbid(unsafe_code)]`.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
