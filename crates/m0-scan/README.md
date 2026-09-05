# m0-scan

The windowed scan the M0 probe measures, written in Rust and compiled to wasm32 by the toolchain a decoder would be built with.

The probe has always compared a chunked scan against a flat one to work out what the windowed control flow costs. It did that with a module written by hand in `wat`, which addresses every load as a base plus an index because that is how a person writes a chunked loop. The flat loop it was compared against addresses each load with a single register. A real decoder is compiled from Rust by a toolchain that gets to choose how to express the same arithmetic, and it may well choose better, so an overhead measured against the hand written pair is partly a measurement of the way the probe expresses windowing.

This crate is the same measurement in the other shape. The probe runs both and reports both, and the hand written module stays, because two shapes that disagree is itself a result and dropping the one that produced the surprising number is the wrong way to resolve a surprise.

Nothing installs this. It is not published, it has no dependencies, and it is meaningless outside `crates/iris-vm/examples/m0_probe.rs`, which builds it for wasm32 every time it runs rather than reading a checked in binary.

## The exports

`reserve(len) -> addr` makes room for `len` bytes and hands back the address they start at, because the host writes the bytes to be scanned straight into linear memory.

`sum_flat(len)` sums them in one pass. That is the denominator.

`sum_chunked(chunks, win, stride)` sums `chunks` windows of `win` bytes, advancing by `stride` and calling the host between each. With `stride` equal to `win` it walks forward through the buffer, which isolates the control flow. With `stride` at zero it reads the same window every time, which is the shape where the host refills that window between chunks.

Both go through one `sum_range`, so the bytes are summed by the same code either way and the only difference between the two is the chunk bookkeeping and the host call. If each had its own loop, a compiler that vectorised one and not the other would show up as an abstraction cost, which is exactly the confusion this crate exists to remove.
