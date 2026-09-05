# iris-source

Range oriented data sources for iris.

A decoder declares the byte ranges it needs and the host serves them. That inversion is what lets the same decoder run against a local file, a page cache, and an object store.

`RangeSource` is the trait that inversion is expressed as. Asking it for a range never waits: it either hands back the bytes or says they are not here yet, and the host does something else in between, which is what a single threaded host needs and what the resumable path in the sandbox is built on. A host that does not mind blocking calls `read_blocking` and gets the ordinary shape back in one line.

Three implementations ship with it. `MemorySource` over bytes that are already resident, `FileSource` over a local file read through the window below, and `ObjectSource` over an object store, behind the `object-store` feature so that a host reading local files does not link an HTTP stack to do it. They are interchangeable because they all pass the same suite, which lives in the library behind the `conformance` feature rather than in this crate's tests, so that a fourth implementation written in another repository can be held to exactly the same promises.

`Segment` is not a fourth implementation but an adapter over any of them. It presents a byte range of one source as a source addressed from zero, which is how a decoder is shown the data section of a container and nothing else when the container is too large to hold.

`Readahead` is the other adapter. A decoder walking a column asks for it in the pieces the encoding is written in, and over a network those pieces are forty round trips where one would do, so this fetches a block at a time and serves the run out of it. It keeps a few blocks rather than one, because a columnar scan reads a piece of each column in turn and a single block is thrown away by every turn. How far ahead to read and how many runs to follow are numbers the host picks, and a decoder has no way to influence them or to find out that there are any.

`Window` is the piece underneath the file source. It reserves a fixed span of address space once, maps part of a file into it, and moves that part when a request falls outside it, so the address space a dataset costs is chosen by the host rather than by the size of the data. When the view moves, every address the old view covered stops being readable rather than returning bytes from the part of the file that used to be there, which is the property the whole type exists for: a stale read produces an answer that is wrong and looks right, and nothing downstream can catch it.

This is the only crate in the workspace that contains unsafe code. Every other one carries `#![forbid(unsafe_code)]`.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
