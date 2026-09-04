# Roadmap

Nine milestones, each finished when its exit gate passes rather than when the code is written. Every gate is something a machine can check, and every gate is an issue in the matching [GitHub milestone](https://github.com/tamnd/iris/milestones).

The ordering is deliberate. The two experiments that could falsify the whole architecture are cheap and come first, before anything is built on top of them. Building six crates and then discovering the ABI shape is wrong is the expensive version of week one.

## M0, prove or kill the two bets

Nothing is built. Two microbenchmarks decide the shape of everything after them: the cost of a `require_range` host call on Wasmtime 48, and the overhead of a sliding window against whole file mapping on a resident local file.

**Gate.** `require_range` under 100 ns per call, and windowing under 3% on a resident file. If the first fails, the design moves to a shared window descriptor before any code is written. If the second fails, the claim that supporting remote storage does not tax the local path is wrong and this plan needs rewriting.

## M1, the ABI and a decoder that does nothing interesting

`iris-abi`, `iris-format`, `iris-decoder`, and enough of `iris-runtime` to run a decoder over a fully resident buffer. No windowing, no validation, no metering, no threads. The first decoder is deliberately trivial, fixed width integers with no compression, because the target is the contract rather than the decoding.

**Gate.** A decoder built from the `iris-decoder` macro produces a correct `RecordBatch`, checked against arrow-rs. The negotiation path is exercised: a host at ABI 1 refuses a container declaring ABI 2 with a message naming the required ABI, the decoder digest and the schema, not a parse error. A decoder compiled against a four field request prefix runs correctly when handed a full request, which is the only test that directly checks the property the whole ABI design exists for. `iris-abi` has zero dependencies and builds `no_std`.

## M2, trust

`iris-trust` and `iris-guard`. This milestone is what separates `iris` from a research artifact, and it comes early on purpose. Validation is the price of the security claim, and a project that defers the price has not paid it.

**Gate.** BLAKE3 verification is mandatory and happens before compilation, so a tampered decoder byte fails the scan with both digests in the message. `iris-guard` rejects every array in an adversarial corpus: an offset one past the end, a null count off by one, a dictionary index equal to the dictionary length, a view buffer index equal to the buffer count, a length times element width that overflows, a child array one row short, and unbounded schema nesting. The guard fuzzer runs 24 hours with no accepted but invalid array, which is the most important gate in the project because it is the only one whose failure mode is silent. Embedded decoders are the default and a decoder referenced by URI does not execute without an explicit opt in. Epoch metering is on by default and a decoder that loops forever traps within the deadline and names its digest. The cost of `iris-guard` is measured across the full platform matrix and written into the design notes, against a decision rule that was committed before the measurement.

## M3, portability

`iris-vm` on Linux, macOS and Windows, with continuous integration on all three plus 64 bit Arm Linux.

**Gate.** The M1 and M2 suites pass on Linux x86-64, macOS on Apple silicon, Windows x86-64, and Linux arm64. The window fuzzer finds no stale read across a window slide on any platform: a pointer held across a slide traps or reads zeroes, and never returns stale data. Windows placeholder API sliding survives repeated slide and remap cycles, which is the highest risk unwritten code in the design. The minimum supported Rust version is 1.98 and CI enforces it. No nightly, no vendored forks, and `cargo tree` shows no patched dependency.

## M4, ranges and windowing

`iris-source` with a file source, a memory source and an object store source, the `require_range` path with resumption, and host side coalescing and prefetch.

**Gate.** A decoder reads a file larger than 4 GiB correctly through a 256 MiB window. The same decoder binary, unmodified, reads from a local file and from an S3 compatible endpoint. Unmodified is the gate: if the decoder needs a recompile then the abstraction has leaked. A resident local file stays within 3% of the M1 whole buffer path, which re-checks the M0 bet against real code. The resumable path works from a single threaded host that never blocks. Request count and bytes transferred are reported per scan, because wall clock alone hides the mechanism.

## M5, a real decoder and the measurements that matter

A production grade decoder for something worth decoding. BtrBlocks is the first target: its 16 MiB chunked layout is exactly the shape `require_range` is designed for, and the literature describes it as good but unadopted, which is the thesis demonstrated.

**Gate.** Byte identical output against the reference BtrBlocks implementation over the full conformance corpus. Projection pushdown reaches storage, so reading three of forty columns from an object store transfers roughly three fortieths of the object. The remote scan comparison is published with the hardware, the endpoint and the network characteristics stated next to the numbers. The arm64 against x86-64 vectorisation comparison is run, which is the most interesting unrun experiment in this area and the one that could produce a new result rather than a reimplementation. Idempotence and statelessness harnesses pass: every request replayed twice byte identically, and batches requested in shuffled order concatenate to the sequential result.

## M6, concurrency and the DataFusion integration

A global memory pool, genuinely `Send` jobs, `loom` coverage, and `iris-df`.

**Gate.** A decode job is `Send` with no `unsafe impl` and no thread identity assertion anywhere in the tree, so the class of bug is structurally impossible rather than fixed. A DataFusion query over an `iris` table survives Tokio work stealing under a thousand iteration stress run. `loom` passes on the pool handoff and eviction paths, enabled by a CI configuration flag and never by a cargo feature. An asynchronous `require_range` miss yields the worker thread instead of parking it.

## M7, the native fast path

`iris-native`, keyed by content hash, with its differential harness. This is deferred to here even though the WebAssembly vector width ceiling makes it required infrastructure, for one reason: M5's arm64 result may show the gap is x86 only, which changes both the priority and the kernel set. Building it before knowing that is building the wrong thing quickly.

**Gate.** Registering a native kernel without a passing byte identical differential run is a build failure, not a warning. Keying is on the content hash, so a decoder with a different hash claiming the same name gets the WebAssembly path. Substitution is logged with both digests on every scan. The ahead of time compilation cache is keyed on the decoder digest, the Wasmtime version, the target triple and the configuration hash, with cold start numbers published.

## M8, integrations and the adoption path

A C ABI, Python bindings, a DuckDB extension, and the Parquet embedding.

**Gate.** The C ABI installs and runs on a machine that has never seen it, on all three desktop platforms. An `iris` decoder embedded in Parquet file metadata works both ways: readers that do not know about it read the file normally, and readers that do use the embedded decoder. That last item is written up as a proposal to the Parquet community with numbers attached. It is the highest leverage adoption route in the plan and the only one that requires nobody to adopt anything. Parquet v2 encodings shipped a decade ago and remain underused specifically because writers cannot be sure readers support them, which is ossification, measured, in a paper arguing against this approach.

## Decision points

Places to stop and reassess rather than push through.

| After | Question | If the answer is bad |
|---|---|---|
| M0 | Is a host call affordable? | Redesign to a shared window descriptor before writing code |
| M0 | Does windowing tax the local path? | The first bet failed. Two code paths forever, or reconsider the inversion |
| M2 | What does validation cost? | Over 15%, keep it on and make digest pinning the documented normal path. Do not ship it off |
| M5 | Do declared ranges win on object storage? | The main differentiator is gone and the design notes are wrong |
| M5 | Is the vectorisation gap architecture neutral? | If yes, M7 drops in priority and the story gets much better |

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| `iris-guard` is expensive | High | Decision rule committed before the measurement, at M2. Encoded arrays are the cheap case |
| Nobody writes decoders | High | The N times M problem is not demand. The Parquet embedding at M8 is the answer that needs no adoption |
| Wasmtime API churn across major versions | Medium | `iris-vm` is the only crate that touches it, and the rest of the tree does not know it exists |
| ABI 2 turns out to be needed | Medium | It is a project failure to be explained in writing, which is the correct incentive |
| Scope creep into a query engine | Medium | Stated non-goal. `iris` decodes, it does not plan or execute |
| The field converges on a registered encoding set instead | Medium | Not a bad outcome. `iris` can host that decoder, and the two are complements |
