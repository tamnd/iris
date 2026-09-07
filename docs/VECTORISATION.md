# The sandbox penalty across architectures

The M5 question, from issue #32: WebAssembly caps vector width at 128 bits and is going to keep doing that. Arm Neon is 128 bits and AVX2 is 256, so on x86-64 a native decoder can have vectors twice as wide as a sandboxed one can ever have, and on arm64 it has exactly the same width. If the sandbox penalty is mostly a vector width penalty then it should be structurally smaller on arm64. The prior art ran its whole evaluation on one Intel machine, so nobody had checked.

The short answer is that the hypothesis has the direction right and the mechanism wrong, and that the word structurally does not survive either.

The sandbox is cheaper on arm64. It is not cheaper because the host has narrower vectors there. The guest executes about 1.7 times the host's instructions for the same decode, and that number is very nearly the same on both architectures: 1.71 on arm64 and 1.76 on x86-64. The extra work a sandbox costs is an architecture independent quantity. Where the two differ is in what a core does with those extra instructions, and that is a fact about the two parts measured rather than about their instruction sets.

Widening the host from 128 bit vectors to 256 removes six to eleven percent of the host's instructions on this kernel. That is the entire size of the effect the question was about, measured directly, and it is roughly a tenth of the gap it was offered as the explanation for.

And the premise underneath all of it does not hold: WebAssembly SIMD is worth about one times against a scalar guest on both architectures, four measurements, all of them one. Nothing here vectorises inside the guest at all, so the guest is not pressed up against a 128 bit ceiling. It is nowhere near it.

The probe that produced this is `crates/iris-vm/examples/m5_vector.rs`, and the counter script beside it is `ci/vector_counters.py`. Everything below can be reproduced from them.

## What is compared

One decode kernel, built twice, run three ways.

The kernel is `crates/m5-decode`. It reads a BtrBlocks part through `iris-btr`, walks every chunk in it, and folds a sample of the decoded values into a checksum. It is one crate rather than two, so that what is being compared is two builds of one decoder and not two decoders that happen to agree. It is not published and exists only for this.

The three sides are the guest with WebAssembly SIMD enabled, the guest with it disabled, and the same code compiled for the machine and called in process. The two guest sides say whether the guest uses the 128 bits it is allowed. The guest and native pair is the sandbox penalty.

The corpus is `conformance/btrblocks/fixtures`, one part per case, the same parts the conformance suite reads. Twenty two cases covering the schemes a real column takes: bit packing, patched frame of reference, run length, dictionaries, frequency, pseudodecimal, FSST, and the uncompressed and all null and single value edges. Some are large enough for cache behaviour to matter and some are small enough to be dominated by the fixed cost of entering the decoder, and both ends are reported, because a geometric mean over only the big ones would be a different claim.

Every case checks that all three sides returned the same checksum before any of their timings are reported, and a case where they disagree is a failure rather than a row. That is what makes the comparison a comparison.

The checksum samples one value in sixty four rather than folding every value, and the crate documents at length why. The first working version folded everything, and it reported that an uncompressed column, which decodes by copying, takes the same time as a bit packed one, which decodes by shifting and masking. The fold had become the loop: a rotate, an add and a multiply per value, each waiting on the one before it, which costs more per value than a decoder that is doing well. A serial dependency chain vectorises on no architecture, so measuring one and calling the answer a vector width result would have been wrong in a way that looked like a finding. Sampling puts the fold at about one percent of the loop and leaves the decoders as the thing being timed.

## Where the numbers come from

Three kinds of machine, because the three halves of this need different things from a machine.

**Durations need a machine nobody else is using.** That is an Apple M4, ten cores, 24 GiB, macOS 15.8, aarch64. Release profile, which in this workspace is fat LTO and one codegen unit. Four hundred repeats after twenty warmup passes, median reported.

**Instructions per cycle needs a kernel that will let a process read a performance monitoring unit**, which rules out every hosted runner. That is the fleet's eight core box, `epyc-8c-24gb`: an AMD EPYC Processor with IBPB, eight cores, 23 GiB, Linux 6.8, x86-64, with avx2 and without avx512f. It is a shared tenancy machine, so its durations appear below only as within run ratios and never as absolute times, which is the rule the remote scan comparison already follows.

**Instruction counts need one tool on comparable machines on both architectures**, and the fleet is entirely x86-64. So they come from the hosted runners in `.github/workflows/m5.yml`, counted with callgrind, which simulates rather than measures and therefore returns the same count whatever the neighbours are doing. The x86-64 runner is an AMD EPYC 9V74 80-Core Processor on Linux 6.17, and the arm64 runner is Linux 6.17 aarch64, which does not report a model name and whose feature list includes `asimd`, `asimddp`, `sve` and `sve2`. That is what makes an arm64 answer exist at all, and it is also why there are no arm64 instructions per cycle here: a simulator has no pipeline to report on and the runner has no counters to read.

The x86-64 side is measured both ways, which turns out to matter. perf on the fleet machine and callgrind on the hosted one agree to within a quarter of a percent on every count in this document, on two different processors with two different tools. That agreement is the reason the arm64 callgrind numbers can be set beside the x86-64 perf numbers at all.

**On AVX-512.** The fleet machine that produced every cycle count here reports `avx2` and does not report `avx512f`. The Mac has nothing wider than 128 bits. The hosted x86-64 runner is a part that does have AVX-512, and the tuned leg there asks for `x86-64-v3`, which is AVX2, deliberately, because Valgrind cannot decode everything a richer build emits and a counter step that dies counts nothing. So the step measured here is 128 bits to 256 on every leg, and a step to 512 was not measured anywhere. A host with 512 bit vectors would be four times the guest's width rather than twice, and the honest thing to do with the six to eleven percent below is not to multiply it.

## The durations, on arm64

Apple M4, aarch64, baseline, which on this target means Neon and 128 bit vectors. Microseconds per decode of one part.

| Case | Bytes | Guest SIMD | Guest scalar | Native | Guest over native | SIMD over scalar |
|---|---|---|---|---|---|---|
| dbl-all-null | 36 | 2.8 | 2.9 | 1.5 | 1.94 | 0.99 |
| dbl-dict | 10,093 | 22.9 | 22.8 | 21.6 | 1.06 | 1.01 |
| dbl-frequency | 8,883 | 21.3 | 22.6 | 14.8 | 1.44 | 0.94 |
| dbl-one-value | 36 | 2.8 | 2.5 | 1.7 | 1.65 | 1.10 |
| dbl-pseudodecimal | 31,026 | 50.8 | 51.4 | 42.1 | 1.21 | 0.99 |
| dbl-pseudodecimal-some-null | 33,385 | 48.3 | 48.3 | 41.5 | 1.16 | 1.00 |
| dbl-rle | 513 | 3.8 | 4.5 | 2.7 | 1.44 | 0.84 |
| dbl-uncompressed | 65,564 | 3.1 | 3.2 | 1.5 | 2.00 | 0.95 |
| int-all-null | 32 | 2.8 | 3.0 | 1.1 | 2.48 | 0.94 |
| int-bp | 11,560 | 18.3 | 17.6 | 15.3 | 1.20 | 1.04 |
| int-dict | 9,293 | 20.0 | 19.5 | 18.8 | 1.07 | 1.03 |
| int-dict-some-null | 11,652 | 19.9 | 19.5 | 18.7 | 1.06 | 1.02 |
| int-one-value | 32 | 2.9 | 2.5 | 1.2 | 2.41 | 1.15 |
| int-pfor | 8,776 | 19.7 | 18.9 | 16.6 | 1.19 | 1.04 |
| int-rle | 466 | 3.8 | 4.8 | 2.0 | 1.94 | 0.79 |
| int-uncompressed | 32,796 | 3.4 | 3.4 | 1.2 | 2.83 | 1.00 |
| str-all-null | 32 | 14.6 | 14.7 | 7.0 | 2.07 | 0.99 |
| str-dict | 8,685 | 81.5 | 85.0 | 48.0 | 1.70 | 0.96 |
| str-fsst | 80,651 | 549.4 | 552.5 | 368.3 | 1.49 | 0.99 |
| str-fsst-some-null | 83,010 | 556.8 | 557.0 | 357.0 | 1.56 | 1.00 |
| str-one-value | 58 | 79.8 | 75.5 | 42.5 | 1.88 | 1.06 |
| str-uncompressed | 191,899 | 47.2 | 43.4 | 23.8 | 1.98 | 1.09 |

Across the twenty two cases the guest is 1.60 times the host, geometric mean, worst case 2.83 and best case 1.06. A second run of the same build gave 1.60 again, so the headline is stable to the two digits it is quoted at.

Built with `target-cpu=native` the same machine gives 1.53. Both builds are 128 bit Neon, because Neon is mandatory in the baseline and there is nothing wider on the part, so that pair is a control rather than an experiment: it says what the ratio does when the tuning changes and the vector width does not. It moves by about four percent.

WebAssembly SIMD is worth 0.99 times against the scalar guest, and 0.97 at the other tuning.

## The instruction counts, on both architectures

callgrind, hosted runners, per decode of one part. The counts are simulated, so they are exact and repeat to the digit.

| Case | Architecture | Tuning | Guest SIMD | Guest scalar | Native | Guest over native |
|---|---|---|---|---|---|---|
| int-bp | arm64 | baseline, Neon 128 | 580,111 | 580,116 | 363,367 | 1.60 |
| int-bp | arm64 | cortex-a72, Neon 128 | 580,119 | 580,127 | 359,538 | 1.61 |
| int-bp | x86-64 | baseline, sse2 128 | 620,850 | 620,910 | 369,890 | 1.68 |
| int-bp | x86-64 | x86-64-v3, avx2 256 | 620,819 | 620,885 | 340,178 | 1.82 |
| dbl-pseudodecimal | arm64 | baseline, Neon 128 | 1,574,336 | 1,588,763 | 982,553 | 1.60 |
| dbl-pseudodecimal | arm64 | cortex-a72, Neon 128 | 1,574,343 | 1,588,771 | 990,877 | 1.59 |
| dbl-pseudodecimal | x86-64 | baseline, sse2 128 | 1,790,517 | 1,821,308 | 1,026,914 | 1.74 |
| dbl-pseudodecimal | x86-64 | x86-64-v3, avx2 256 | 1,790,499 | 1,821,286 | 967,901 | 1.85 |
| str-fsst | arm64 | baseline, Neon 128 | 7,787,333 | 7,787,326 | 4,003,600 | 1.95 |
| str-fsst | arm64 | cortex-a72, Neon 128 | 7,787,334 | 7,787,350 | 3,988,017 | 1.95 |
| str-fsst | x86-64 | baseline, sse2 128 | 7,469,606 | 7,469,663 | 4,020,364 | 1.86 |
| str-fsst | x86-64 | x86-64-v3, avx2 256 | 7,469,583 | 7,469,654 | 3,562,716 | 2.10 |

Geometric mean of the guest over native ratio, over those three schemes:

| Architecture | Baseline | Tuned | Change |
|---|---|---|---|
| arm64 | 1.707 | 1.711 | 1.002 |
| x86-64 | 1.758 | 1.920 | 1.092 |

That is the whole experiment in four numbers. On arm64 the tuning changes and the vector width does not, and the ratio moves by two parts in a thousand. On x86-64 the tuning changes and the vector width doubles, and the ratio moves by nine percent. The arm64 row is what makes the x86-64 row attributable to the vector width rather than to having changed the compiler flags at all.

What the tuning did to each side on its own, which is where the nine percent comes from:

| Case | Architecture | Native instructions, baseline to tuned | Guest instructions, baseline to tuned |
|---|---|---|---|
| int-bp | arm64 | 363,367 to 359,538, 0.989 | 580,111 to 580,119, 1.000 |
| dbl-pseudodecimal | arm64 | 982,553 to 990,877, 1.008 | 1,574,336 to 1,574,343, 1.000 |
| str-fsst | arm64 | 4,003,600 to 3,988,017, 0.996 | 7,787,333 to 7,787,334, 1.000 |
| int-bp | x86-64 | 369,890 to 340,178, 0.920 | 620,850 to 620,819, 1.000 |
| dbl-pseudodecimal | x86-64 | 1,026,914 to 967,901, 0.943 | 1,790,517 to 1,790,499, 1.000 |
| str-fsst | x86-64 | 4,020,364 to 3,562,716, 0.886 | 7,469,606 to 7,469,583, 1.000 |

The guest column is 1.000 six times over, to six significant figures, and it has to be: Wasmtime compiles the guest at run time from what it finds on the processor, and a flag on the embedding binary does not reach Cranelift. That column is not a finding, it is the check that says the subtraction underneath these numbers is doing what it claims.

## Instructions per cycle, on x86-64

`epyc-8c-24gb`, perf, per decode of one part, smallest of three rounds kept. Instruction counts held to within 0.04 percent across rounds and cycle counts to within 7 percent, which is why instructions per cycle is quoted here at all and why the script refuses to quote it when they do not.

Baseline, sse2, 128 bit vectors:

| Case | Side | Instructions | Cycles | Per cycle |
|---|---|---|---|---|
| int-bp | guest SIMD | 621,074 | 204,103 | 3.04 |
| int-bp | guest scalar | 621,134 | 202,069 | 3.07 |
| int-bp | native | 369,836 | 105,254 | 3.51 |
| dbl-pseudodecimal | guest SIMD | 1,791,648 | 593,882 | 3.02 |
| dbl-pseudodecimal | guest scalar | 1,822,438 | 590,488 | 3.09 |
| dbl-pseudodecimal | native | 1,026,818 | 306,867 | 3.35 |
| str-fsst | guest SIMD | 7,478,788 | 3,775,843 | 1.98 |
| str-fsst | guest scalar | 7,479,059 | 3,843,468 | 1.95 |
| str-fsst | native | 4,011,152 | 2,319,344 | 1.73 |

With `target-cpu=native`, which on that part is avx2 and 256 bit vectors:

| Case | Side | Instructions | Cycles | Per cycle |
|---|---|---|---|---|
| int-bp | guest SIMD | 621,028 | 198,379 | 3.13 |
| int-bp | guest scalar | 621,066 | 194,357 | 3.20 |
| int-bp | native | 340,077 | 94,429 | 3.60 |
| dbl-pseudodecimal | guest SIMD | 1,791,565 | 570,418 | 3.14 |
| dbl-pseudodecimal | guest scalar | 1,822,362 | 581,982 | 3.13 |
| dbl-pseudodecimal | native | 968,077 | 299,530 | 3.23 |
| str-fsst | guest SIMD | 7,479,072 | 3,821,996 | 1.96 |
| str-fsst | guest scalar | 7,478,919 | 3,849,389 | 1.94 |
| str-fsst | native | 3,556,488 | 2,171,524 | 1.64 |

The same rows as ratios of the guest over the host, which is what the two tables are for:

| Case | Host vectors | Instructions | Cycles | Per cycle |
|---|---|---|---|---|
| int-bp | 128 | 1.68 | 1.94 | 0.87 |
| int-bp | 256 | 1.83 | 2.10 | 0.87 |
| dbl-pseudodecimal | 128 | 1.74 | 1.94 | 0.90 |
| dbl-pseudodecimal | 256 | 1.85 | 1.90 | 0.97 |
| str-fsst | 128 | 1.86 | 1.63 | 1.14 |
| str-fsst | 256 | 2.10 | 1.76 | 1.20 |

Those instruction counts and the callgrind ones in the previous section are the same measurement taken twice, on two different processors, with a hardware counter and with a simulator. They agree to within a quarter of a percent on all twelve figures.

The corresponding within run duration ratios on that machine are 2.06 at 128 bits and 2.40 at 256, geometric mean over all twenty two cases. Those are ratios on a shared machine, offered as shape rather than as evidence, and the counters above are the evidence.

## What it says

**The gap is instructions, not throughput.** On x86-64 at 128 bits the guest runs 1.68 to 1.86 times the host's instructions and retires them at 0.87 to 1.14 times the host's rate. The instruction term is the large one on every scheme measured, and the pipeline term is small, sometimes nothing, and on FSST runs the other way. The sandbox is not making the machine work badly. It is asking the machine to do more work, which is what a bounds check on every access and a linear memory that has to be re-based look like from outside. That is a different finding from a stall and it has a different fix.

**The extra work is very nearly the same on both architectures.** 1.71 on arm64 against 1.76 on x86-64, one tool, comparable machines. The sandbox's tax, counted in instructions, is not an architecture dependent quantity, and there is no sense in which arm64 is being asked to do less.

**So the arm64 advantage is in absorbing the extra instructions, not in being asked for fewer.** The M4 turns a 1.71 instruction ratio into a 1.29 duration ratio on the same three schemes. The fleet's EPYC turns a 1.76 instruction ratio into a 1.83 cycle ratio. One core swallows the sandbox's extra work nearly for free and the other pays for it about one for one. That is a real and useful difference and it is a statement about those two parts. An Apple M4 and a server EPYC differ in far more than their instruction sets, and nothing here separates the two.

**Doubling the host's vector width buys the host six to eleven percent.** sse2 to avx2 takes the native side from 369,890 instructions to 340,178 on bit packing, 1,026,914 to 967,901 on pseudodecimal, and 4,020,364 to 3,562,716 on FSST. In cycles it is two to ten percent. Measured directly, on the machine, with the arm64 control alongside showing the same change of tuning at unchanged width moving nothing. It is about a tenth of the sandbox gap it was proposed as the explanation for.

**FSST is the case where the guest issues better than the host.** 1.98 instructions per cycle against 1.73. A symbol table walk is pointer chasing over unpredictable branches, so the host spends cycles waiting rather than issuing, and the guest's extra bounds checks are cheap, predictable, and fill slots the host was leaving empty. The guest still loses on the clock, because 1.86 times the instructions at 1.14 times the rate is 1.63 times the cycles. It is worth saying because it is the shape of a decoder whose cost is memory rather than arithmetic, and more of the schemes a real column uses are that shape than are not.

**And the guest never uses its own vectors.** WebAssembly SIMD is worth 0.99 and 0.97 times on arm64 by duration, and by instruction count 1.00, 1.00, 0.98 and 0.98 across the four legs. LLVM is not auto vectorising these decoders to `simd128`, which is a known weak spot and not a surprise, but it means the premise underneath the question does not hold. The guest is not held back by a 128 bit cap it is pressed against. It is nowhere near the cap, and a comparison of ceilings is a comparison of two numbers only one side is anywhere near.

**Arm is not structurally at 128 bits either.** The issue's reasoning was that Neon is 128 bits and will stay there, so an arm64 host has no wider vectors available to it. The hosted runner that produced the arm64 counts above advertises `sve` and `sve2`, whose vector length is implementation defined and which ships at 256 bits on parts available today. The measurements here are all Neon, because the baseline target does not enable SVE, but the word structurally was doing real work in the hypothesis and it should not have been. The ceiling on the host side is a property of the part, not of the architecture.

**So the answer to #32 is yes on the number and no on the reason.** The penalty is smaller on arm64. The vector width difference accounts for roughly a tenth of it, the guest is not using the vectors it already has, and Arm's own future parts are not committed to staying narrow. Anyone reaching for arm64 to reduce the sandbox cost will get some of what they came for, and the design note that says they get it because WebAssembly cannot reach AVX-512 is not what the machine says.

**Where this points the work.** The addressable item is the instruction count, and the instruction count is bounds checking and memory base reloading rather than arithmetic. Wasmtime's knobs for exactly that are the guard page configuration and the static memory bound, which trade address space for elided checks, and this probe leaves both at their defaults. Measuring those is a smaller and more useful piece of work than either hand porting decoders to `simd128` or moving the fleet to arm64, and ruling out the vector width answer is what frees it up.

## What this does not settle

**No AVX-512 step.** Everything here is 128 against 256. A part with 512 bit vectors would be a wider step than the one measured, no fleet machine has one, and the hosted runner that does cannot be counted at that width because Valgrind stops on the instructions. So the six to eleven percent should not be extrapolated by multiplying. A machine with AVX-512 and a readable performance monitoring unit is what would settle it, and perf rather than callgrind is how it would have to be done.

**No arm64 instructions per cycle.** There is no arm64 Linux machine in the fleet, macOS has no perf, and a hosted runner has no performance monitoring unit to read. Everything above about the pipeline is an x86-64 statement, and the arm64 half is instruction counts only. One arm64 Linux box in the fleet closes that, and it is the single largest missing piece here.

**Two machines, not two architectures.** The arm64 durations are an Apple M4 and the cycle counts are an AMD EPYC. Comparing a ratio taken on one against a ratio taken on the other folds in every difference between those parts. That is exactly why the argument rests on the within machine tuning pairs and on instruction counts taken with one tool on comparable runners, and not on holding 1.60 up against 2.06.

**One kernel.** These are BtrBlocks decoders and nothing else. A decoder written to vectorise, in a scheme that vectorises, on data that vectorises, would answer this differently. The honest reading of the SIMD numbers is that this kernel does not exercise the mechanism the question was about, rather than that the mechanism does not exist.

**Three schemes out of twenty two carry the counter tables.** Bit packing because it is the one that ought to vectorise, pseudodecimal because it is arithmetic over doubles, and FSST because it is a symbol table walk that should not vectorise anywhere. The spread was chosen deliberately, and callgrind at around fifty times the run time is why it is three and not twenty two.

**One Wasmtime version at its defaults.** Wasmtime 48, no tuning of guard pages or memory bounds. A large part of the instruction gap is expected to be sensitive to precisely those, which is the point of the last paragraph of the previous section and not something measured here.

## Reproducing it

The durations, on any machine, twice:

```
cargo run --release -p iris-vm --example m5_vector -- --repeats 400
RUSTFLAGS="-C target-cpu=native" cargo run --release -p iris-vm --example m5_vector -- --repeats 400
```

The pair is the measurement and either one on its own is not, which is why the probe says so at the bottom of its own output. `--json` emits the run as one object, `--cases NAME` narrows to one fixture, and `--only SIDE` runs one of the three.

The counters, on a machine that can count:

```
ci/vector_counters.py --cases int-bp
```

It picks perf where the kernel will expose a performance monitoring unit and callgrind where it will not, says which it used and why, and exits 3 when neither can count, which is a fact about the machine rather than a failure. It sizes its own repeat counts from a warm up run, takes each measurement three times and keeps the smallest, and leaves instructions per cycle out entirely when the cycle counts moved between rounds by more than the effect being measured. `RUSTFLAGS` is passed through untouched, and giving each tuning its own `CARGO_TARGET_DIR` avoids rebuilding the world twice.

Note that callgrind decodes the instruction stream with its own decoder rather than the processor's, so a build tuned for the exact machine can emit something it has never been taught and stop on it. That is why the workflow names its targets instead of asking for `native`.

The full matrix is `.github/workflows/m5.yml`, two architectures by two tunings under callgrind, uploading the JSON behind every number in this document. Nothing in it fails a build on a number.
