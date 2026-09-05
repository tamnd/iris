# Contributing to iris

Thanks for looking. This is an early project, so the most useful contributions right now are arguments about the design rather than code.

## Before you start

Read `docs/ROADMAP.md` and the milestone your change belongs to. If your change does not belong to a milestone, open an issue first and say what it is for. The repository is organised around exit gates, not around a backlog, and a change that does not move a gate is usually better as a discussion.

## The three rules that are not negotiable

**ABI changes get more scrutiny than anything else.** The ABI in `iris-abi` is the one surface here that can ossify, because once decoders exist in the wild they cannot be recompiled on demand. Any change to it needs a written compatibility note explaining what an old decoder does when it meets a new host and what a new decoder does when it meets an old host. "It will not happen in practice" is not a compatibility note.

**A performance claim needs a measurement.** Numbers in documents, commit messages and pull request descriptions must cite a claim identifier from [`tamnd/iris-bench`](https://github.com/tamnd/iris-bench), which carries the run identifier, the confidence interval, the sample size and the machine class. If the number has not been measured yet, write `[verify]` and say what falsifies it. CI checks that every `[verify]` marker either stays a marker or cites a claim identifier that exists.

**Safety at the sandbox boundary is not an optimisation target.** `iris-guard` validates the Arrow arrays a sandboxed decoder returns. Removing or bypassing a check because it costs time needs a separate proposal with the threat model attached, not a commit that says the check was redundant.

## Practical things

Run this before you push. It is the same set CI runs, and running it locally is faster than waiting.

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
python3 ci/discipline.py
```

That last one is the checks a compiler cannot do. A nightly toolchain in a workflow, a patched dependency in a manifest and a machine's hostname in a document all make the build work rather than break it, so each of them needs something that fails on it instead. It reads files and takes a second.

If you are touching the platform code in `iris-source`, add the platform you are not on. `cargo check` cross compiles without a linker for the other target, so a Windows path can be compile checked from a Mac and a Unix path from Windows, and it catches a wrong module or a missing feature in seconds instead of in a CI run.

```
rustup target add x86_64-pc-windows-msvc
cargo clippy -p iris-source --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
```

This checks and does not run. The tests still have to run on a real machine of that kind, which is what the CI matrix is for.

The toolchain is pinned in `rust-toolchain.toml` to Rust 1.98, which is also the minimum supported version, and the discipline check enforces that those two numbers are the same one. Raising it is a deliberate change, not something that happens because a dependency wanted it.

Some further expectations, none of them surprising:

- Public items are documented. `missing_docs` is on.
- Every `unsafe` block has a comment saying why it is sound. `undocumented_unsafe_blocks` is denied, so CI will tell you if you forget.
- Avoid `unwrap` and `expect` in library code. They are fine in tests and in the CLI where the failure is the point.
- New dependencies need a reason in the pull request. `cargo deny` enforces the licence and advisory policy in `deny.toml`, and additions that pull a second TLS stack or a C toolchain into a data format library will be pushed back on.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/). The release notes are generated from them.

## Pull requests

Small and focused beats large and complete. A pull request that changes the ABI, the trust model and a decoder in one go will sit unreviewed because nobody can hold all three in their head at once.

Describe what you measured, not only what you changed. If your change is meant to make something faster, the pull request should say by how much, on what, and how many times you ran it.

Draft pull requests are welcome, especially for design questions. Say what you are unsure about in the description.

## Reporting a security issue

Do not open a public issue. See `SECURITY.md`.

## Licence

By contributing you agree that your contribution is licensed under the Apache License 2.0, matching the rest of the repository.
