# The sandbox penalty across architectures

The M5 question, from issue #32: WebAssembly caps vector width at 128 bits and is going to keep doing that. Arm Neon is 128 bits and AVX2 is 256, so on x86-64 a native decoder can have vectors twice as wide as a sandboxed one can ever have, and on arm64 it has exactly the same width. If the sandbox penalty is mostly a vector width penalty then it should be structurally smaller on arm64. The prior art ran its whole evaluation on one Intel machine, so nobody had checked.

The short answer is that the hypothesis has the direction right and the mechanism wrong. The sandbox does cost less on arm64. It is not because the host has narrower vectors there. Doubling the host's vector width from 128 to 256 bits on x86-64 removes between six and eleven percent of the host's instructions on this kernel, and the sandbox gap is between sixty and one hundred and ten percent. What the gap is made of is instructions: the guest executes 1.7 to 2.1 times as many of them for the same decode, and it issues them about as well as the host does and on one scheme better. Vector width is a term in this rather than the term, and a small one.

The probe that produced this is `crates/iris-vm/examples/m5_vector.rs`, and the counter script beside it is `ci/vector_counters.py`. Everything below can be reproduced from them.

## What is compared

One decode kernel, built twice, and run three ways.

The kernel is `crates/m5-decode`. It reads a BtrBlocks part through `iris-btr`, walks every chunk in it, and folds a sample of the decoded values into a checksum. It is one crate rather than two, so that what is being compared is two builds of one decoder and not two decoders that happen to agree. The crate is not published and exists only for this.

The three sides are the guest with WebAssembly SIMD enabled, the guest with it disabled, and the same code compiled for the machine and called in process. The two guest sides are what says whether the guest uses the 128 bits it is allowed. The guest and native pair is the sandbox penalty.

The corpus is `conformance/btrblocks/fixtures`, one part per case, the same parts the conformance suite reads. Twenty two cases covering the schemes a real column takes: bit packing, patched frame of reference, run length, dictionaries, frequency, pseudodecimal, FSST, and the uncompressed and all null and single value edges. Two of them are large enough to matter for cache behaviour and several are small enough to be dominated by the fixed cost of entering the decoder, and both ends are reported because a geometric mean over only the big ones would be a different claim.

Every case checks that all three sides returned the same checksum before any of their timings are reported, and a case where they disagree is a failure rather than a row. That is what makes the comparison a comparison.

The checksum samples one value in sixty four rather than folding every value, and the crate documents at length why. The first working version folded everything, and it reported that an uncompressed column, which decodes by copying, takes the same time as a bit packed one, which decodes by shifting and masking. The fold had become the loop: a rotate, an add and a multiply per value, each waiting on the one before it, which costs more per value than a decoder that is doing well. A serial dependency chain vectorises on no architecture, so measuring one and calling the answer a vector width result would have been wrong in a way that looked like a finding. Sampling puts the fold at about one percent of the loop and leaves the decoders as the thing being timed.

## Where the numbers come from

Two kinds of machine, because the two halves of the answer need different things from a machine.

Durations need a machine nobody else is using. That is an Apple M4, ten cores, 24 GiB, macOS 15.8, aarch64. Release profile, which in this workspace is fat LTO and one codegen unit. Four hundred repeats after twenty warmup passes, median reported.

Instruction counts and instructions per cycle need a kernel that will let a process read a performance monitoring unit, which rules out every hosted runner and every virtual machine that is not configured for it. That is the fleet's eight core box, `epyc-8c-24gb`: an AMD EPYC Processor with IBPB, eight cores, 23 GiB, Linux 6.8, x86-64, with avx2 and without avx512f. It is a shared tenancy machine, so its durations are quoted only as within run ratios and never as absolute times, which is the same rule the remote scan comparison follows.

There is no arm64 Linux machine in the fleet, and macOS has no perf. So the arm64 instruction counts come from the hosted `ubuntu-24.04-arm` runner in `.github/workflows/m5.yml`, counted with callgrind, which simulates rather than measures and therefore returns the same instruction count whatever the neighbours on that machine are doing. That is what makes an arm64 answer possible at all, and it is also why there are no arm64 instructions per cycle below: a simulator has no pipeline to report on, and the hosted runner has no counters to read.

The same workflow counts hosted x86-64 with the same tool, so that the cross architecture instruction comparison is one tool on two comparable machines rather than two tools on two different ones.

**No AVX-512 anywhere.** Not in the fleet, not on the hosted runners, not on the Mac. A 512-bit host would have four times the guest's vector width rather than twice, so every x86-64 gap below is a lower bound on what such a machine would show. The size of that correction can be estimated from the 128 to 256 step measured here, and estimating it is not measuring it.

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

Across the twenty two cases the guest is 1.60 times the host, geometric mean, worst case 2.83 and best case 1.06. A second run of the same build gave 1.60 again, with the worst and best cases at 2.72 and 1.07, so the headline is stable to the two digits it is quoted at.

Built with `target-cpu=native` instead, the same machine gives 1.53. Both builds are 128 bit Neon, because Neon is mandatory in the baseline and there is nothing wider on the part, so this pair is the control rather than the experiment: it says what the ratio does when the tuning changes and the vector width does not. It moves by about four percent, in the direction of a smaller gap, which is the extra instruction selection that the Apple scheduling model buys the host being worth slightly less than nothing on this code.

WebAssembly SIMD is worth 0.99 times against the scalar guest, geometric mean, and 0.97 at the other tuning. That is the single most useful number on this page and it is easy to walk past. **Nothing in this kernel vectorises inside the guest.** The guest is not using the 128 bits WebAssembly gives it, so an argument about the host having 128 or 256 bits to beat it with is an argument about a race one side is not running.

## The instruction counts, on x86-64

`epyc-8c-24gb`, perf, per decode of one part, smallest of three rounds kept. Instruction counts held to within 0.04 percent across rounds and cycle counts to within 7 percent, which is why instructions per cycle is quoted here at all.

Baseline, which on this target is sse2 and 128 bit vectors.

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

With `target-cpu=native`, which on this part is avx2 and 256 bit vectors.

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

The same three cases as ratios of the guest over the host, which is what the two tables are for.

| Case | Host vectors | Instructions | Cycles | Per cycle |
|---|---|---|---|---|
| int-bp | 128 | 1.68 | 1.94 | 0.87 |
| int-bp | 256 | 1.83 | 2.10 | 0.87 |
| dbl-pseudodecimal | 128 | 1.74 | 1.94 | 0.90 |
| dbl-pseudodecimal | 256 | 1.85 | 1.90 | 0.97 |
| str-fsst | 128 | 1.86 | 1.63 | 1.14 |
| str-fsst | 256 | 2.10 | 1.76 | 1.20 |

The guest's instruction count is the same to within a hundredth of a percent whichever tuning the host was built at, which it has to be: Wasmtime compiles the guest at run time from what it finds on the processor, and a flag on the embedding binary does not reach Cranelift. That is not a finding, it is the check that says the subtraction is doing what it claims.

The corresponding within run duration ratios on that machine are 2.06 at 128 bits and 2.40 at 256, geometric mean over all twenty two cases. Those are ratios on a shared machine and are reported as shape rather than as evidence, and the counters above are the evidence.

## What it says

**The gap is instructions, not throughput.** On the 128 bit build the guest runs 1.68 to 1.86 times the host's instructions and retires them at 0.87 to 1.14 times the host's rate. The instruction term is the large one on every scheme measured and the pipeline term is small, sometimes zero, and on FSST it runs the wrong way. So the sandbox is not making the machine work badly. It is asking the machine to do more work, which is what a bounds check on every memory access and a linear memory that has to be re-based looks like from outside. That is a different finding from a stall, and it has a different fix.

**FSST is the case where the guest issues better than the host.** 1.98 instructions per cycle against 1.73. A symbol table walk is pointer chasing over unpredictable branches, so the host spends its cycles waiting rather than issuing, and the guest's extra bounds checks are cheap, predictable, and fill slots the host was leaving empty. The guest still loses on the clock, because 1.86 times the instructions at 1.14 times the rate is 1.63 times the cycles. It is worth saying because it is the shape of a decoder whose cost is memory rather than arithmetic, and the schemes a real column uses are increasingly that shape.

**Doubling the host's vector width buys the host six to eleven percent.** Going from sse2 to avx2 takes the native side from 369,836 instructions to 340,077 on bit packing, from 1,026,818 to 968,077 on pseudodecimal, and from 4,011,152 to 3,556,488 on FSST. In cycles it is two to ten percent. That is the entire size of the vector width effect on this kernel, measured directly and on the machine, and it is roughly a tenth of the sandbox gap it was proposed as the explanation for.

**And the guest is not using its own vectors at all.** WebAssembly SIMD is worth 0.99 and 0.97 times on arm64 and 0.96 and 0.97 times on x86-64. Four numbers, two architectures, all of them one. LLVM is not auto vectorising these decoders to `simd128`, which is a known weak spot and not a surprise, but it means the premise underneath the whole question does not hold: the guest is not held back by a 128 bit cap it is pressed up against, it is nowhere near the cap.

**So the answer to #32 is yes on the number and no on the reason.** The penalty is smaller on arm64. It is smaller because of what the two hosts and the two Wasmtime backends do with the same code, and the vector width difference between them accounts for about a tenth of it. Anyone reaching for arm64 to reduce the sandbox cost will get some of what they came for, and the design note that says they get it because WebAssembly cannot use AVX-512 is not what the machine says.

**Where that points the work.** The addressable item is the instruction count, and the instruction count is bounds checking and memory base reloading rather than arithmetic. Wasmtime's existing knobs for that are the guard page configuration and the static memory bound, which trade address space for elided checks and which this probe leaves at their defaults. Measuring those is a smaller and more useful piece of work than either porting decoders to `simd128` by hand or moving the fleet to arm64, and it is what the vector width answer above frees up by ruling out.

## What this does not settle

**No AVX-512, so the x86-64 half is a lower bound.** Everything above is 128 against 256. A machine with 512 bit vectors would be a wider step than the one measured, and the six to eleven percent figure should not be extrapolated to it by multiplying.

**One kernel.** These are BtrBlocks decoders and nothing else. A decoder written to vectorise, in a scheme that vectorises, on data that vectorises, would answer this differently, and the honest reading of the SIMD numbers above is that this kernel does not exercise the mechanism the question is about rather than that the mechanism does not exist.

**Three schemes out of twenty two carry the counter tables.** Bit packing because it is the one that ought to vectorise, pseudodecimal because it is arithmetic over doubles, and FSST because it is a symbol table walk that should not vectorise anywhere. That spread was chosen deliberately, and callgrind at fifty times the run time is why it is three and not twenty two.

**No arm64 instructions per cycle.** There is no arm64 Linux machine in the fleet and no performance monitoring unit on a hosted runner, so the arm64 half of this is instruction counts only. Everything above about the pipeline is an x86-64 statement. Putting one arm64 Linux box in the fleet is what closes that, and it is the single largest missing piece here.

**One Wasmtime version and its default configuration.** Wasmtime 48, defaults throughout, no tuning of guard pages or memory bounds. A large part of the instruction gap is expected to be sensitive to exactly those, which is the point of the paragraph above and not something this report measured.

**Two machines and two architectures, not two architectures.** The arm64 durations are an Apple M4 and the x86-64 counters are an AMD EPYC. Comparing a ratio taken on one against a ratio taken on the other folds in every difference between those two parts, not just their instruction sets. That is exactly why the argument here rests on the within machine tuning pair and on instruction counts taken with one tool on comparable runners, and not on holding 1.60 up against 2.06.

## Reproducing it

The durations, on any machine, twice:

```
cargo run --release -p iris-vm --example m5_vector -- --repeats 400
RUSTFLAGS="-C target-cpu=native" cargo run --release -p iris-vm --example m5_vector -- --repeats 400
```

The pair is the measurement and either one on its own is not, which is why the probe says so at the bottom of its own output. `--json` emits the same run as one object, `--cases NAME` narrows it to one fixture, and `--only SIDE` runs one of the three.

The counters, on a machine that can count:

```
ci/vector_counters.py --cases int-bp
```

It picks perf where the kernel will expose a performance monitoring unit and callgrind where it will not, says which it used and why, and exits 3 when neither can count, which is a fact about the machine rather than a failure. `RUSTFLAGS` is passed through untouched, and giving each tuning its own `CARGO_TARGET_DIR` avoids rebuilding the world twice.

The full matrix is `.github/workflows/m5.yml`, which runs two architectures by two tunings under callgrind and uploads the JSON. Nothing in it fails a build on a number.
