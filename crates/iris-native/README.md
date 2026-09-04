# iris-native

Native fast path implementations for known decoders.

The WebAssembly vector width is capped at 128 bits and will be for the foreseeable future, so a host that recognises a decoder should be able to skip the sandbox. This is that table.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
