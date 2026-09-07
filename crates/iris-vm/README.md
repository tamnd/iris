# iris-vm

WebAssembly execution layer for iris decoders.

Wraps Wasmtime. Owns instantiation, execution metering, the state page, and the sliding window that lets a wasm32 guest address a dataset larger than four gibibytes.

Every call into a decoder is metered, and there is no way to ask for one that is not. A decoder that loops forever costs the query it was running and nothing else: the call comes back naming the decoder by digest and saying how long it had, and the host thread that made it gets control back. `Vm::with_deadline` moves the budget, which is the only knob there is. The default is ten seconds, which no decoder reading a resident buffer will ever notice.

`Vm::with_compilation_cache` names a directory to keep compiled decoders in, which is off until somebody names one. Compiling is most of what opening a container costs and the result is the same every time, so the second process to open a decoder reads it instead. The key covers the module and everything about the engine that decides what compiling it produces, so an entry built by a different Wasmtime or for a different target is never found rather than being found and rejected. Every way it can fail ends in an ordinary compile.

The window and the state page are still ahead.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
