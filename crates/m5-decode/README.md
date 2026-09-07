# m5-decode

The BtrBlocks decode kernel the M5 vectorisation probe runs on both sides of the sandbox.

This crate is not published and is not useful on its own. It wraps `iris-btr` in one function that decodes a column part and folds the result into a checksum, and it is built twice: to wasm32 as a `cdylib` that the probe runs under Wasmtime, and to the host as an `rlib` that the probe calls directly. Both builds run the same source, which is what makes the ratio between them a statement about the sandbox rather than about two different pieces of code.

See `crates/iris-vm/examples/m5_vector.rs` for the probe and `docs/VECTORISATION.md` for what it found.
