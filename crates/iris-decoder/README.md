# iris-decoder

Guest side SDK for writing iris decoders.

A decoder is a `wasm32-unknown-unknown` module that speaks the iris ABI. This crate hides the record encoding and the host imports behind a macro so that writing a decoder is mostly writing the decode loop.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
