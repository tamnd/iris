# What opening a container costs

The remote scan report ended with a loose end. About thirty milliseconds of every open went on hashing the decoder section and compiling it to machine code, there was nowhere to keep the result, and every process paid it again. It called that the single largest addressable item in its table. This document is what happened when it was addressed.

The short answer is that a warm compilation cache takes almost all of it. An open goes from tens of milliseconds to a fraction of one, and what is left is close to the floor a process reaches when the decoder is already compiled in memory. The first open after a deployment costs a little more than it used to, because it writes the artefact, and that is the only price.

The probe that produced this is `crates/iris-runtime/examples/cold_start.rs`. Everything below can be reproduced from it.

## What was compared

One container, opened four ways, timed from the call to `Runtime::open` to the point where the schema is in hand. Nothing here scans. A scan would fold a fixed cost per open into a number that grows with the rows, and what a host feels is exactly this cost divided by the work of the query that followed it.

`no cache` is a fresh `Runtime` with nowhere to keep a compiled decoder. This is what every process start cost before any of this existed.

`cold` is a fresh `Runtime` against an empty directory. It compiles, serialises what it compiled, writes that, and loads it back. This is the first start after a deployment, and it is reported rather than folded away because it is more work than `no cache` and anybody turning this on should see the price.

`warm` is a fresh `Runtime` against a directory that already holds the artefact. This is every start after the first, and it is the number the whole thing is for.

`pooled` is one `Runtime` opening the container a second time, so the decoder is already compiled in this process and nothing is looked up. It is the floor. There is no arrangement of caches that gets an open below it, and it is here to say how much of what is left in `warm` is the container rather than the decoder.

Each of the first three builds a new `Runtime` for every repeat, which is the nearest thing a probe has to a new process: a fresh Wasmtime engine with an empty pool, which is the state a restart leaves.

The container holds the `fixedwidth` decoder, built from `crates/iris-decoder/examples/fixedwidth.rs` on the way in. The fixture is 1,000 rows of 4 `int64` columns and the size does not matter, because opening reads the trailer, the header, the footer and the decoder section and never touches the data.

## Where the numbers come from

Apple M4, ten cores, 24 GiB, macOS 15.8, aarch64. Release profile, which in this workspace is fat LTO and one codegen unit. The cache directory is under `target`, so it is on the same disk the build writes to.

Twenty five repeats per shape, median reported, with the fastest and slowest of the twenty five next to it.

The spread on the compiling shapes is wide and it is worth saying why rather than presenting a tidy median. Wasmtime compiles a module across several threads, so `no cache` and `cold` are the two shapes that care what else the machine is doing, and a laptop is never entirely idle. The two shapes that the result rests on, `warm` and `pooled`, are single threaded reads and barely move.

The run below is the quietest of several, and the others are the reason the caveat is here rather than a footnote. On a loaded machine `no cache` reached 242 ms while `warm` stayed at two tenths of a millisecond, so the busier the host the better the cache looks. Reporting the quietest run therefore reports the smallest saving that was seen, which is the right way round for a claim like this one.

## The numbers

| Shape | Median ms | Spread ms |
|---|---|---|
| no cache | 30.41 | 29.64 to 33.70 |
| cold | 32.78 | 31.37 to 34.06 |
| warm | 0.13 | 0.13 to 0.16 |
| pooled | 0.04 | 0.04 to 0.04 |

The decoder module is 73,263 bytes and the artefact it compiles to is 230,832 bytes, which is 3.2 times as big.

## What it says

**A warm open saves 30.28 ms of 30.41, which is 99.6 percent.** Compiling was essentially the whole of it. Hashing the decoder section, parsing the footer and negotiating the handshake add up to the tenth of a millisecond that is left.

**The first open costs 2.4 ms more than it used to.** That is serialising the compiled module and writing 230 kilobytes, and it is paid once per decoder per directory rather than once per process. A host that starts twice has already won it back many times over.

**What is left over the in memory floor is 0.09 ms.** That is reading 230 kilobytes off the page cache and letting Wasmtime map it, against having the module already in hand. So the on disk cache is roughly three times the cost of the in memory one and roughly two hundred times cheaper than compiling, which is why the two are layered rather than being alternatives: an open asks the pool first and only a miss there reaches the directory.

**The artefact is 3.2 times the module.** That ratio is a property of the engine rather than of the decoder, which is the same reasoning the decoder pool uses to weigh entries in module bytes instead of compiled ones. For sizing a directory, a decoder is a few hundred kilobytes and a host that has seen a hundred distinct decoders has spent about twenty megabytes.

**The remote scan number moves.** That report measured a windowed open over object storage at 34.2 ms, of which about three were the network and about thirty one were local work. Those thirty one are what this removes, so the same open against a warm cache is around four milliseconds, and the projected scan it sits inside goes from about 125 ms to about 95 ms. That is arithmetic on the two tables rather than a re-measurement, and re-running the remote scan probe with a cache configured is the way to confirm it.

## The key

An entry is machine code for one target compiled by one compiler. An entry reached by a runtime that would have compiled it differently is not a stale answer, it is the wrong instructions, so the key has to make that impossible rather than merely unlikely.

The key is a digest over a tag, the digest of the decoder module, and the fingerprint of the engine. The fingerprint is Wasmtime's own `precompile_compatibility_hash`, which covers the target triple, the compiler flags, the flags of the instruction set being targeted, the tunables, the WebAssembly features that are on, and the version of Wasmtime itself. So all four of the components the milestone asked for are in there: the decoder, the Wasmtime version, the target triple and the configuration.

Those three come out of Wasmtime rather than out of a list maintained on this side. Wasmtime is the one that knows which of its settings reach the compiler, and a list kept here would be a list that is quietly wrong for one release after every upgrade. What iris adds is width. The components are collected rather than mixed down to a machine word and hashed once at the end, so two different compilers colliding is not something the cache has to think about.

An upgrade of Wasmtime, a change of target or a change of settings therefore misses every entry that was there and fills the directory again. Nothing is invalidated because nothing needs to be: the old entries are simply unreachable.

What is deliberately not in the key is the deadline. That is the host's patience rather than a compiler setting, so two hosts that disagree about it share entries and the module is stamped with the reader's own budget on the way past.

## What it does not settle

**Where the directory comes from.** An entry is machine code and loading one maps it executable, so a directory another user can write into is a directory that can hand the process anything. This is off by default and turning it on is an operator's decision, the same shape as allowing a decoder from outside the container. iris does not pick a location, does not have a default one, and does not check who owns the one it is given.

**Nothing collects it.** Entries are written and never removed, so a directory grows with the number of distinct decoders a host has ever opened, and an upgrade leaves the old generation behind unreachable. At a few hundred kilobytes each that is slow, and it is still unbounded. Deleting the directory is safe at any moment, which makes a periodic sweep an easy thing for an operator to arrange and is why there is no policy here yet.

**One decoder.** `fixedwidth` is small. A larger decoder compiles for longer, which makes the saving larger, and produces a larger artefact, which makes the warm read slower. Both move in the direction that keeps the conclusion, but the ratio would be different.

**One filesystem, warm.** Every warm open in this table read a file that had just been written and was therefore in the page cache. A genuinely cold read off a slow disk is not measured here, though 230 kilobytes is a small enough read that it is hard to see it approaching thirty milliseconds.

**Two processes at once.** Writes go to a temporary name and are renamed into place, so a reader never sees a partial file and two processes compiling the same decoder end with one of the two artefacts rather than halves of both. That is reasoned about rather than measured, and the tests cover the shapes it produces rather than the race that produces them.

## Reproducing it

```
cargo run --release -p iris-runtime --example cold_start
```

`--rows` and `--repeats` move the fixture and the sample count. `--json` emits the same run as one object, which is the shape iris-bench takes.

Turning it on in a host is one call:

```rust
let runtime = Runtime::new()?.with_compilation_cache("/var/cache/iris/decoders");
```

`Runtime::compilations_reused` and `Runtime::compilations_stored` are how a host checks it is working, and they are the only way it says so. Every failure in the cache falls back to compiling, silently and on purpose, so a host where reused stays at zero across restarts has a directory it cannot read or cannot write.
