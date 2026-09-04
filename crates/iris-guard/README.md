# iris-guard

Structural validation of Arrow arrays crossing the sandbox boundary.

A sandbox stops a decoder from reading the host's memory. It does nothing at all about the numbers the decoder hands back, and those numbers are offsets, lengths and buffer indices that the host is about to use to read its own memory. That gap is the difference between a security claim and a security property, and this crate is where the gap gets closed.

If `check` returns `Ok` then every offset in the batch is inside the buffer it indexes, every buffer is long enough for the number of slots its array claims, and every child array is long enough for the parent that points into it. That is a bounds property and nothing more. Whether a string column holds well formed UTF-8 is a correctness question, and it is left to Arrow, because reading a badly encoded string cannot leave the buffer.

The adversarial corpus is in `src/corpus.rs` rather than in a test file, so that it can be extended by anybody writing a host or a decoder against this ABI, and so that the sound cases sit next to the unsound ones. A checker that refuses everything passes every adversarial corpus ever written.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
