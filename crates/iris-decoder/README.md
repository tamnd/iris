# iris-decoder

Guest side SDK for writing iris decoders.

A decoder is a `wasm32-unknown-unknown` module that speaks the iris ABI. This crate hides the record encoding and the host imports behind a macro so that writing a decoder is mostly writing the decode loop.

Implement `Decoder`, which is three associated constants and two methods, then call `export_decoder!` once. A decoder does not encode records, does not negotiate a version, does not touch guest memory by address and does not know which WebAssembly functions the host calls. It says what capabilities it needs, opens, and decodes.

The macro is deliberately thin. Everything it could have generated lives in `Instance` instead, so the thing the host will actually call can be driven from a normal test with `Instance`, `Resident` and `Collect`, without a host and without a WebAssembly runtime. See `examples/passthrough.rs`, which is both a real exported decoder and a `main` that runs it natively.

There is no unsafe code in this crate. The host never hands the guest an address and the guest never follows one, so a host that lies produces a wrong answer rather than a corrupt guest. That costs a copy of the source, which is the right trade while the source is resident, and the file that changes when it stops being the right trade at M4 is `src/guest.rs` rather than anybody's decoder.

The wire format, the negotiation and the exported function table are written down in [docs/ABI.md](../../docs/ABI.md).

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
