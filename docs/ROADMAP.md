# Roadmap

Nine milestones, each finished when its exit gate passes rather than when the code is written. Every gate is something a machine can check, and every gate is an issue in the matching [GitHub milestone](https://github.com/tamnd/iris/milestones).

The ordering is deliberate. The two experiments that could falsify the whole architecture are cheap and come first, before anything is built on top of them. Building six crates and then discovering the ABI shape is wrong is the expensive version of week one.

## M0, prove or kill the two bets

Nothing is built. Two microbenchmarks decide the shape of everything after them: the cost of a `require_range` host call on Wasmtime 48, and the overhead of a sliding window against whole file mapping on a resident local file.

**Gate.** `require_range` under 100 ns per call, and windowing under 3% on a resident file. If the first fails, the design moves to a shared window descriptor before any code is written. If the second fails, the claim that supporting remote storage does not tax the local path is wrong and this plan needs rewriting.

### The M0 decision

The probe is `crates/iris-vm/examples/m0_probe.rs` and the workflow that runs it on every supported platform is `.github/workflows/m0.yml`. Wasmtime 48.0.1, release profile, medians with 95 percent bootstrap intervals.

The four hosted rows below are shared machines, so the magnitudes are not publishable and are not published. What they are good for is the shape of the answer and whether it changes character on a different architecture, which turns out to be the whole story here. The last row is an Apple M4 laptop with 24 GiB, which is not a shared machine.

**Bet one, the cost of a host call.** The number is the cost of one call, taken as the difference between a loop that calls an imported function and the same loop without the call, with the callee doing the work a real `require_range` has to do.

| Machine | Cost of one host call | Gate |
| --- | --- | --- |
| Hosted `ubuntu-24.04-arm` | 3.43 ns | pass |
| Hosted `ubuntu-24.04` | 4.68 ns | pass |
| Hosted `macos-latest` | 2.67 ns | pass |
| Hosted `windows-latest` | 4.02 ns | pass |
| Apple M4 laptop | 3.41 ns | pass |

The gate was 100 ns and the worst number is 4.68 ns, which is a margin of twenty. The range inversion is affordable and there is no reason to move to a shared window descriptor. That is the first bet settled, in the direction the design assumed, on every platform.

Worth saying plainly, because it is the more interesting half of the result: a Wasmtime host call is not expensive. Most of the intuition that it would be comes from a time when it was.

**Bet two, what the window costs.** Two numbers here. The first is the abstraction, a scan through the windowed control flow with a host call per chunk that does nothing, against a flat scan of the same bytes in the same order. That is the gate, because it isolates the cost of the shape from the cost of moving data. The second is a real refill, where the host copies the next window into guest memory between chunks.

| Machine | Abstraction | Gate | With a real refill |
| --- | --- | --- | --- |
| Hosted `ubuntu-24.04-arm` | -3.7% | pass | +53.8% |
| Hosted `macos-latest` | +2.6% | pass | +50.6% |
| Apple M4 laptop | +1.5% | pass | +25.5% |
| Hosted `windows-latest` | +15.7% | fail | +116.0% |
| Hosted `ubuntu-24.04` | +49.1% | fail | +141.5% |

This is not a pass and it is not a failure. It splits along architecture: the gate passes on every aarch64 machine measured and fails on both x86-64 machines, and on the x86-64 Linux runner it fails by a factor of sixteen rather than by a margin. The confidence intervals on that row do not overlap at all, so whatever it is, it is not sampling noise on the day.

**What is decided.** Bet one is settled and closed. Bet two is not, and the honest thing is to say so rather than to take one of the two branches the plan wrote in advance.

Neither branch is taken yet, for two reasons. The first is that both failing rows are shared hosted machines and the passing rows include the only machine in the table that is not shared, so the split could be architecture or could be neighbours. The second is that the windowed loop in the probe is hand written `wat` that addresses each load as a base plus an index, and a real decoder emitted by a real toolchain does not necessarily produce that shape, so some of the gap may be the probe rather than the design.

**What changes.** Three things, and none of them is moving the gate. The gate was written down before the measurement, which is the only order in which a gate means anything, and a gate that moves when it is inconvenient is not a gate.

1. M4 does not start until the x86-64 result has been reproduced on hardware nobody else is using. Of the machines this project has, one is eligible: the Intel Core i9-13900K. The three AMD EPYC boxes are shared tenancy virtual machines, so they cannot produce a number with a duration in it. If it reproduces, the abstraction cost is real and the design has to answer it. If it does not, the hosted rows were noise and M4 proceeds as written. That is issue #65, and neither of those two things is what happened. The last section below is what did.
2. The naive refill is already ruled out as an implementation. A host memcpy of the window between chunks costs between a quarter and one and a half times the scan itself, on every machine measured including the ones that pass the abstraction gate. Whatever windowing ships, the bytes do not get copied into guest memory a window at a time. That is a real finding and it is the useful half of the second measurement.
3. The probe grows a second windowed shape, compiled from Rust rather than written by hand, so that the next run of this measurement separates the cost of the design from the cost of the way the probe expresses it. That is issue #66, and it is done. The next section is what it found.

The numbers above go into iris-bench and get attached to claim identifiers once the reproduction machinery in B3 exists. Until then they live here, in the document that has to justify them.

### The second shape

The probe now measures the window twice, in two shapes, and reports both. One is the pair of loops written by hand in `wat` that produced the table above. The other is `crates/m0-scan`, compiled to wasm32 by the toolchain a decoder is written with, where the flat and the chunked loop both go through one summing function so that the only difference between them is the chunk bookkeeping and the host call.

**The gate is judged against the compiled shape.** A decoder is Rust compiled to wasm32, so that is the loop that will actually run, and a gate applied to a loop nobody will ever execute is a gate on the probe. The order that was decided in matters, because choosing the more flattering of two numbers after seeing both is how a gate stops meaning anything: the argument is the one in issue #66, written down before either number existed, and the gate itself does not move. Three percent is still three percent, and both shapes are reported on every run whatever they say.

Here is the same four hosted platforms, 256 MiB through a 16 MiB window, both shapes side by side, from run 33959663573:

| Machine | Hand written | Gate | Compiled | Gate | Compiled flat scan |
| --- | --- | --- | --- | --- | --- |
| Hosted `ubuntu-24.04-arm` | -3.2% | pass | -0.3% | pass | 13.57 ms |
| Hosted `macos-latest` | +1.0% | pass | +2.0% | pass | 12.33 ms |
| Hosted `windows-latest` | +37.4% | fail | -0.2% | pass | 16.34 ms |
| Hosted `ubuntu-24.04` | +38.9% | fail | +3.0% | pass | 14.27 ms |

**The architecture split was the probe.** Both x86-64 machines fail the gate by a factor of twelve in the hand written shape and pass it in the compiled one. There is no split left: every platform passes, and on three of the four the interval on the windowed scan overlaps the interval on the flat scan, so the overhead is not distinguishable from zero. The one row that does not overlap is `ubuntu-24.04` at 14.27 ms flat against 14.69 ms windowed, which is +3.0% and sits directly on the gate.

A second run of the same four platforms, 33960133056, says the same thing and puts that last row somewhere less interesting: the compiled shape comes out at +1.4, -0.6, +0.7 and +0.1 percent, and the hand written shape fails again on both x86-64 machines at +47.2 and +24.9 percent. So the +3.0 percent above is the noisy end of the compiled result rather than a real position on the gate, and the hand written failure is the reproducible part.

So the answer to the question the M0 decision left open is that the hand written chunked loop is bad code on x86-64 specifically, and a decoder does not contain it. Reading each load as a base plus an index costs nothing on aarch64, where the compiled flat scan is nearly twice as fast as the hand written one and the hand written pair still passed, and it costs 38 percent on x86-64, where the compiled flat scan is barely faster at all and the hand written pair failed. The penalty was never in the flat scan or in the windowing. It was in the one loop that had to add two registers per load.

The second finding is that ruling out the naive refill got stronger rather than weaker. The copy costs what it costs in milliseconds either way, so dividing it by a faster scan makes it a larger fraction. On this run the refill costs between 67 and 91 percent of the compiled flat scan on all four platforms, against 36 to 121 percent of the hand written one. Finding number two above stands, and it stands harder.

**What this does to bet two.** It is not closed. What it no longer is, is a question about whether the design has an x86-64 problem, because the shape a decoder actually has does not have one on either x86-64 machine here. What is left is that all eight rows across the two runs are shared hosted machines, so the confirmation on hardware nobody else is using still has to happen. That is #65, and the next section is what happened when it was attempted.

### What the workstation could not measure

The reproduction on unshared hardware is issue #65, and the machine for it is the Intel Core i9-13900K, which is the one machine in the fleet whose timings are supposed to mean anything. Two runs of the probe on it said the compiled shape fails the gate at +4.03% and +3.21%, with the flat and windowed confidence intervals not overlapping either time, while the hand written shape passed at -3.99% and -4.83%. That is the exact inverse of the hosted matrix above, and it was tempting to write it up as the answer.

It is not the answer. Judging a three percent gate on a confidence interval taken from inside one run is the wrong measurement, and it took a third run to see why. The flat scan and the windowed scan are measured one after the other, so anything that moves the machine between the two blocks lands entirely in the difference between them, and an interval computed inside a block cannot see it by construction. What that interval describes is how consistent the samples were while the block was running, which is not the question.

So the probe was run twenty four times in one sitting on that machine, six repeats at each of four window sizes, interleaved rather than blocked so that drift over the quarter hour would spread across all four sizes instead of landing on whichever size happened to be measured while it happened. Run 33963369333:

| Window | Runs | Compiled, median | Compiled, range | Compiled flat scan, median |
| --- | --- | --- | --- | --- |
| 4 MiB | 6 | +2.82% | +0.77% to +8.45% | 35.39 ms |
| 16 MiB | 6 | +2.53% | -8.00% to +4.80% | 30.25 ms |
| 32 MiB | 6 | +1.70% | -0.42% to +17.70% | 34.58 ms |
| 128 MiB | 6 | -0.03% | -0.32% to +5.25% | 30.96 ms |

Across all twenty four the median is +1.09% and the standard deviation is 4.69 percentage points, on a gate set at three. Nine of the twenty four are above the gate and the rest are below it, including two that are below zero by more than the gate is above it. The two earlier runs that said +4.03% and +3.21% were two draws from that.

**The finding is about the machine, not about the window.** This workstation cannot resolve a three percent effect in a single run, and no amount of extra samples inside a run fixes that, because the samples inside a run are not what is varying. The flat scan alone came out at 26.28 ms in the first run and at 35.39 ms in the sweep an hour later, which is a spread of a third on a measurement of the same bytes by the same code on an idle machine that nobody else is using.

Why is not a mystery. It is a hybrid part with eight performance cores and sixteen efficiency cores, the job is not pinned to either kind, it runs under Windows where the boost state and the processor affinity cannot be read by an unprivileged process, and boost is on. A run free to move between a performance core and an efficiency core produces a distribution with two modes, and a confidence interval over two modes describes neither of them. The same reasoning is why the iris-bench eligibility gates cap that machine under Windows at ratios rather than durations: not because the numbers are wrong, but because nothing checked whether the clock was steady, and being unable to see a setting is not evidence that the setting is right.

**What this does to the gate.** The gate does not move. Three percent is still three percent, and the reason the number came out the way it did is a defect in how it was measured rather than an argument about where the line should be.

What changes is that the gate is judged across whole runs and not inside one. Twenty four repeats put the standard error on the median at about one percentage point, so what the fleet can say today is that the compiled windowed shape costs around one percent on this machine and is not distinguishable from three. That is not a pass and it is not a failure. It is the largest claim this hardware supports.

**So #26 is not confirmed reachable and it is not replaced.** The M4 gate stays at three percent on a resident local file, because the threshold was never the problem. The measurement was. What #26 gains is a protocol: it is judged on the median of repeated whole runs with the interval taken across them, and a single run reporting a within-run interval does not settle it in either direction.

M4 is unblocked on that basis. The design question the failing rows seemed to raise turned out not to exist: no shape of windowed loop has been shown to cost anything on x86-64 that survives being measured properly, and the one shape that looked like it did was the hand written probe rather than the design. What is left is a measurement problem, and it belongs to iris-bench rather than here. It is the noise floor issue in B0 and the bare metal Linux issue on the workstation class, and until one of those lands, the fleet's honest resolution on this question is about one percentage point per two dozen runs.

## M1, the ABI and a decoder that does nothing interesting

`iris-abi`, `iris-format`, `iris-decoder`, and enough of `iris-runtime` to run a decoder over a fully resident buffer. No windowing, no validation, no metering, no threads. The first decoder is deliberately trivial, fixed width integers with no compression, because the target is the contract rather than the decoding.

**Gate.** A decoder built from the `iris-decoder` macro produces a correct `RecordBatch`, checked against arrow-rs. The negotiation path is exercised: a host at ABI 1 refuses a container declaring ABI 2 with a message naming the required ABI, the decoder digest and the schema, not a parse error. A decoder compiled against a four field request prefix runs correctly when handed a full request, which is the only test that directly checks the property the whole ABI design exists for. `iris-abi` has zero dependencies and builds `no_std`.

### The M1 result

The gate passes and the milestone is closed at `v0.2.0`. The test is `crates/iris-runtime/tests/gate.rs`, and it compiles `crates/iris-decoder/examples/fixedwidth.rs` for wasm32 every time it runs rather than reading a committed `.wasm`. A checked in fixture would keep passing after the ABI had drifted away from the SDK, which is the one failure this milestone exists to catch.

Two things came out of writing it that were not obvious in advance. The first is that the ABI check belongs before the module is compiled, not in the handshake: a decoder built against a major version this host does not speak is never going to agree on terms, and the container has the decoder's name, its digest and the schema in hand at that point, none of which are in scope by the time a refusal comes back out of a guest. The second is that the schema in that message has to be abbreviated. A real Arrow schema printed in full is pages of nested types and gets truncated by whatever reads the log, so it is the names and types in order, capped, with a count of the rest.

## M2, trust

`iris-trust` and `iris-guard`. This milestone is what separates `iris` from a research artifact, and it comes early on purpose. Validation is the price of the security claim, and a project that defers the price has not paid it.

**Gate.** BLAKE3 verification is mandatory and happens before compilation, so a tampered decoder byte fails the scan with both digests in the message. `iris-guard` rejects every array in an adversarial corpus: an offset one past the end, a null count off by one, a dictionary index equal to the dictionary length, a view buffer index equal to the buffer count, a length times element width that overflows, a child array one row short, and unbounded schema nesting. The guard fuzzer runs 24 hours with no accepted but invalid array, which is the most important gate in the project because it is the only one whose failure mode is silent. Embedded decoders are the default and a decoder referenced by URI does not execute without an explicit opt in. Epoch metering is on by default and a decoder that loops forever traps within the deadline and names its digest. The cost of `iris-guard` is measured across the full platform matrix and written into the design notes, against a decision rule that was committed before the measurement.

**What the guard costs.** Registered as claim C0001 in the `iris-bench` ledger, with the three band rule fixed in the first commit of this repository and the measurement taken afterwards. The guard is divided by assembling the same batch into Arrow arrays, which is the tightest denominator there is and therefore the one that shows the guard in its worst light. A scan also decodes inside the sandbox and that is where the time in a real workload goes, so each share below is an upper bound on the share of a scan by some margin.

| Platform | int64-plain | int64-nullable | utf8 |
|---|---|---|---|
| linux-x86_64 | 0.41% | 5.87% | 23.10% |
| linux-aarch64 | 0.90% | 2.50% | 22.63% |
| macos-aarch64 | 5.61% | 6.56% | 17.38% |
| windows-x86_64 | 0.47% | 2.92% | 22.08% |

Strings land over fifteen percent on every platform, so the rule says the guard stays on and its cost is a design problem rather than a check to remove. Nothing about that is surprising once it is written down: checking a string column means looking at every offset, and under half a nanosecond per offset is close to the floor for a bounds check per element. What the design problem actually is, is that the offsets are checked twice, once here and once inside Arrow, and the second pass cannot be dropped without building arrays unchecked. Fixing it properly means the guard producing something Arrow will accept without re-validating, which is a change to how arrays are built and belongs with the window work rather than here.

Two things came out of measuring it. The first is that most of what the guard appeared to cost was not checking. Counting nulls masked and bounds tested every byte, which is the one shape a compiler cannot vectorise, and it was fifty four percent of assembling a nullable batch by itself before it was rewritten as a popcount. Reading offsets tested the offset width once per offset. Both were a branch per element rather than a check per element, and the measurement is what found them.

The second is that the note in the issue, that encoded arrays would be the cheap case, is wrong. A dictionary of 256 values behind 8192 keys is half the bytes of the column it stands for and costs 28 to 86 times more to check, because a key is a position into something else and every one has to be looked at, while a plain column of fixed width values is a single multiplication and no bit pattern of eight bytes can be out of range. The guard charges for indirection, not for bytes. That is worth knowing before the container format starts carrying dictionaries, and it is why strings are the expensive shape here.

## M3, portability

`iris-vm` on Linux, macOS and Windows, with continuous integration on all three plus 64 bit Arm Linux.

**Gate.** The M1 and M2 suites pass on Linux x86-64, macOS on Apple silicon, Windows x86-64, and Linux arm64. The window fuzzer finds no stale read across a window slide on any platform: a pointer held across a slide traps or reads zeroes, and never returns stale data. Windows placeholder API sliding survives repeated slide and remap cycles, which is the highest risk unwritten code in the design. The minimum supported Rust version is 1.98 and CI enforces it. No nightly, no vendored forks, and `cargo tree` shows no patched dependency.

### The window

`iris_source::Window` reserves a span of address space once and slides a view of a file inside it. It lives in `iris-source` rather than in `iris-vm`, even though the milestone issues are labelled for the virtual machine, because it is the only code in the tree that needs `unsafe` and putting it here is what lets every other crate keep `#![forbid(unsafe_code)]`. The crate whose job is "a mapped file" is also the honest home for the code that maps a file.

The two platforms hold the address range in different ways and that is the whole difficulty. Unix replaces a mapping atomically with `MAP_FIXED`, so a view is removed by mapping `PROT_NONE` over it rather than by unmapping it, because an unmapped range is a hole another thread's allocation can land in. Windows has no atomic replace and uses placeholders instead: reserve the range, split a piece off it, put a view in the piece, take the view out and leave the piece, rejoin the pieces. Five flags in a fixed order, and getting the order wrong does not fail, it leaks the reservation or leaves a split that the next map trips over a thousand cycles later.

Two alignment numbers rather than one, which is the easiest thing here to get wrong on a machine where they are equal. A mapping offset has to be a multiple of the allocation granularity, sixty four kibibytes on Windows and the page size everywhere else. A mapping length has to be a multiple of the page size. Rounding a length up by the allocation granularity looks correct on Unix and fails on Windows at the end of a file, because the pages past the end of the section are not part of it.

Each of the three gate assertions was checked by breaking the property it exists to catch, because an assertion nobody has seen fail is a comment. The stale read check was verified by making the Unix unmap a no op, the address space check by skipping the unmap in `Drop`, which failed at round 254 of 400, and the handle check by leaking one descriptor per cycle, which failed at five handles before the loop and four thousand and five after. Two of the three were inadequate as first written and were rewritten because of it: the address space test reserved sixteen gibibytes in total, which a sixty four bit process does not notice, and the handle test only opened one more file at the end, which succeeds with four thousand handles leaked.

Windows found three more things that nothing else could have. The first is that a view may not run past the end of the section, and a section made from a file is exactly as long as the file, so the last view of a file whose length is not a multiple of the page size asks for more than there is and gets `ERROR_ACCESS_DENIED`, which reads like a permissions problem and is not one. Unix has nothing to say about this because `mmap` rounds a length up itself and the rest of the last page reads as zero. The placeholder is still split at the rounded up length, because the view occupies that many pages of address space either way, and only the number handed to `MapViewOfFile3` comes down.

The second is that the test pattern was wrong. It was two multipliers and a shift, and it repeated every sixty four kibibytes, because the shifted term only kept bits that depend on the offset modulo two to the sixteen. Every view starts on a multiple of the allocation granularity, which on Windows is exactly sixty four kibibytes, so the bytes at the start of one view were identical to the bytes at the start of every other view, and the assertion that a slide replaced them could not tell a correct remap from no remap at all. It is now an avalanche mix. A test pattern for this needs a mixer rather than an arithmetic sequence, and the platform whose granularity happens to line up with the period is the one that says so.

A third one is about the shape of the API rather than about a bug. A view starts on an allocation boundary, so a span of exactly one allocation unit serves a request only when it happens not to straddle one, and a span for a largest request of `n` wants to be at least `n` plus one unit. The stress test asked for a span of sixty four kibibytes, which is many units on Unix and exactly one on Windows, and was refused two thousand cycles in for a reason that had nothing to do with what it was testing.

`fuzz_window` is the third fuzz target and it found something in its first minute: a zero length read at exactly the end of the file, where the view that would cover it is empty and both platforms refuse a mapping of no bytes. That is what a decoder produces when it asks for a column that happens to be empty and happens to sit last in the file. The stress test walks one fixed stride, which is the access pattern a scan has and the one worth being sure about on all four platforms on every change. The fuzzer is for the order nobody thought of.

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
| M0 | Is a host call affordable? | Redesign to a shared window descriptor before writing code. **Settled: yes, 2.7 to 4.7 ns against a 100 ns gate** |
| M0 | Does windowing tax the local path? | The first bet failed. Two code paths forever, or reconsider the inversion. **Open: passes on aarch64, fails on x86-64, being reproduced on hardware that is not shared** |
| M2 | What does validation cost? | Over 15%, keep it on and make digest pinning the documented normal path. Do not ship it off. **Settled: 17 to 23% on strings against the tightest denominator there is, under 7% on everything else, on all four platforms. It stays on, iris-bench claim C0001** |
| M5 | Do declared ranges win on object storage? | The main differentiator is gone and the design notes are wrong |
| M5 | Is the vectorisation gap architecture neutral? | If yes, M7 drops in priority and the story gets much better |

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| `iris-guard` is expensive | High | Decision rule committed before the measurement, at M2. Measured: strings 17 to 23%, everything else under 7%, and the encoded case turned out to be the expensive one rather than the cheap one |
| Nobody writes decoders | High | The N times M problem is not demand. The Parquet embedding at M8 is the answer that needs no adoption |
| Wasmtime API churn across major versions | Medium | `iris-vm` is the only crate that touches it, and the rest of the tree does not know it exists |
| ABI 2 turns out to be needed | Medium | It is a project failure to be explained in writing, which is the correct incentive |
| Scope creep into a query engine | Medium | Stated non-goal. `iris` decodes, it does not plan or execute |
| The field converges on a registered encoding set instead | Medium | Not a bad outcome. `iris` can host that decoder, and the two are complements |
