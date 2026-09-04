# iris-vm

WebAssembly execution layer for iris decoders.

Wraps Wasmtime. Owns instantiation, execution metering, the state page, and the sliding window that lets a wasm32 guest address a dataset larger than four gibibytes.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
