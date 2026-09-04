# Bootstrap

How to get a clean checkout building and testing on each supported platform. If you follow this and something does not work, that is a bug in this document and it is worth an issue.

The four supported platforms are Linux x86-64, Linux arm64, macOS on Apple silicon, and Windows x86-64. They are all in CI and they are all expected to stay green.

## What you need

Rust 1.98 or later. The repository pins the toolchain in `rust-toolchain.toml`, so if you have rustup installed then the correct toolchain, the correct components and the `wasm32-unknown-unknown` target are all fetched the first time you run cargo. You do not need to install them by hand.

A C toolchain. Wasmtime builds Cranelift, which needs a linker and a working `cc`. On Linux that is the usual build essentials package, on macOS it is the Xcode command line tools, on Windows it is the Visual Studio Build Tools with the C++ workload.

That is the whole list. There is no vendored dependency, no patched crate, and no nightly toolchain outside the Miri and fuzzing jobs, which is a deliberate constraint and not an accident. See `docs/ROADMAP.md` under M3.

## Linux x86-64 and Linux arm64

Install rustup if you do not have it:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Install the C toolchain. On Debian and Ubuntu:

```
sudo apt-get update && sudo apt-get install -y build-essential pkg-config
```

Then build and test:

```
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
```

Both architectures are identical from here. The only difference worth knowing about is that the wasm probe numbers differ, which is the point of the M0 measurements rather than a problem.

## macOS on Apple silicon

Install the command line tools, then rustup:

```
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then the same two cargo commands as above.

macOS is a development platform here. It is in CI and the tests must pass on it, but no published measurement comes from it, because a laptop with a thermal budget and a browser open is not a measurement environment. That is written down in the iris-bench fleet notes rather than left as folklore.

## Windows x86-64

Install the Visual Studio Build Tools with the "Desktop development with C++" workload, then rustup from `https://rustup.rs`. Use the MSVC toolchain, not the GNU one. Then:

```
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
```

Windows is the platform most likely to break, because reserving address space and mapping views into parts of it works differently enough there to need its own implementation. That work is M3 and the tests that cover it are expected to be the fiddliest in the repository.

## The wasm32 target

Decoders compile to `wasm32-unknown-unknown`. The toolchain file adds the target, so this should already work:

```
cargo build -p iris-abi -p iris-decoder --target wasm32-unknown-unknown --locked
```

If it does not, run `rustup target add wasm32-unknown-unknown` and open an issue, because the toolchain file was supposed to handle it.

You do not need a wasm runtime installed. Wasmtime is a library dependency and it is built from source as part of the workspace.

## Running the checks CI runs

The full set, in the order CI runs them:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
cargo doc --workspace --no-deps --all-features --locked
```

The lint set is strict on purpose and it is worth reading `[workspace.lints]` in the root `Cargo.toml` before arguing with it. Two of those lints are `deny` rather than `warn`, both about unsafe code, and neither is negotiable.

## Running the M0 probe

The M0 measurements have their own harness:

```
cargo run --release -p iris-vm --example m0_probe -- --json
```

It prints a JSON object with a median, a bootstrap confidence interval and a sample size for each measurement. What those numbers mean, and what they decide, is in `docs/ROADMAP.md`.

## First build times

Cranelift is a large dependency and the first release build takes a while, several minutes on a laptop and longer on a small virtual machine. After that the incremental builds are quick. If you are on a machine with less than about 4 GB of memory, build with `-j 2` or the linker will be the thing that fails rather than the compiler.

## Editor setup

Nothing special. rust-analyzer works out of the box. The repository has no editor configuration checked in and no plan to add any, because that is a matter of personal preference and not a matter of correctness.
