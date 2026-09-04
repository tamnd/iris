# iris

Ship the decoder with the data.

`iris` is a self-decoding dataset runtime written in Rust. A dataset carries a reference to a WebAssembly decoder alongside its bytes, and any host that can run WebAssembly can read the dataset without linking a format-specific reader. The point is to stop paying the N times M cost of wiring N data systems to M storage formats, and to stop letting the set of readable formats freeze in place because writing a new reader means writing it once per engine.

The idea comes from AnyBlox (Gienieczko, Kuschewski, Neumann, Leis, Giceva, PVLDB 18(11), best paper at VLDB 2025). `iris` is not a fork of it. It is a from-scratch implementation with a different I/O model, a different ABI, and a validation layer at the sandbox boundary, built after a survey of the work that cites AnyBlox and the work it competes with. The design notes live in `docs/`.

## Status

Pre-alpha. Nothing works yet. The repository exists so the milestones and their exit gates are public from day one, and so the numbers this design claims can be checked against a measurement harness that is developed in the open next to it.

Every performance claim here is settled in [`tamnd/iris-bench`](https://github.com/tamnd/iris-bench), a separate repository that reproduces the published figures of the papers involved and re-runs the public benchmarks against every rival. Claims that have not been measured yet are marked and stay marked.

## What is different from the prior art

**Ranges instead of whole-file mapping.** AnyBlox maps the entire dataset into the guest's linear memory before the decoder runs, which is fast and also means the decoder cannot prefetch, cannot express a layout, and cannot read from an object store at all. In `iris` the decoder declares the byte ranges it needs and the host serves them, so the same decoder works against a local file, a page cache, and S3. The host keeps ownership of I/O, which is where scheduling and caching decisions belong.

**A sliding window, not a 4 GiB ceiling.** wasm32 gives a decoder 4 GiB of address space. Rather than move to memory64, which costs roughly half again on the hot loop, the host slides a window of the dataset through guest memory and the decoder addresses the dataset in dataset coordinates. Datasets are not limited by the guest address space.

**An ABI designed like a wire protocol.** The ABI is the only surface here that can ossify, so it is length-prefixed records with negotiated capabilities and a defined way to refuse politely, not a fixed argument list with a projection mask that caps a table at 64 columns.

**Validation at the trust boundary.** A sandboxed decoder returns Arrow arrays. Arrow arrays contain offsets, lengths and validity bitmaps that the host is about to trust. `iris-guard` checks them before anything downstream reads them, and the cost of that check is a published number rather than an assumption.

**Execution metering.** A decoder that loops forever should cost a query, not a host thread.

## Layout

| Crate | What it is |
|---|---|
| `iris-abi` | The guest and host ABI. Record layouts, capability negotiation, version rules. No I/O. |
| `iris-decoder` | Guest side SDK. What you write a decoder against. |
| `iris-format` | The bundle and metadata format. Parsing, writing, content digests. |
| `iris-vm` | The WebAssembly execution layer over Wasmtime. Instantiation, metering, the window. |
| `iris-source` | `RangeSource` and its implementations: file, mapped file, object store. |
| `iris-guard` | Structural validation of Arrow arrays crossing the sandbox boundary. |
| `iris-trust` | Decoder identity, content hashes, signature policy, the native substitution table. |
| `iris-runtime` | The thing an engine embeds. Ties the above together into a scan. |
| `iris-native` | Hash-keyed native implementations of decoders the host already knows. |
| `iris-cli` | `iris` the command line tool: inspect, verify, decode, bundle. |

Engine integrations (DataFusion, DuckDB, a C ABI, and Python bindings) are planned and are not in this tree yet. They arrive at M6 and M7.

## Building

Requires Rust 1.98 or newer. The toolchain file pins it.

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Decoders are built for `wasm32-unknown-unknown`:

```
rustup target add wasm32-unknown-unknown
```

## Roadmap

Milestones M0 through M8, each with an exit gate that a machine can check, are tracked as [milestones](https://github.com/tamnd/iris/milestones) with one issue per gate. `docs/ROADMAP.md` has the summary. M0 is a week that can invalidate everything after it, which is the point of running it first.

## The ABI

The host and decoder contract is written down in [`docs/ABI.md`](docs/ABI.md), including the compatibility promise: what is allowed to change without breaking a decoder that is already in the wild, and what is not. It is the only surface in the project that can ossify, so it is the only one with its own document.

## Contributing

See `CONTRIBUTING.md`. The short version: the ABI is the one thing that is hard to change later, so ABI changes get more scrutiny than anything else, and a performance claim needs a run identifier from `iris-bench` before it goes into a document.

## Licence

Apache License 2.0. See `LICENSE`.

## Credit

AnyBlox is the reference this work argues with, and the memory hook, the state page, decoder-by-reference with a content hash, and host-sized batch pull are all its ideas. The paper is worth reading before this repository.
