# Security policy

## Reporting a vulnerability

Report privately through [GitHub Security Advisories](https://github.com/tamnd/iris/security/advisories/new). Please do not open a public issue for a vulnerability.

You should get an acknowledgement within three working days. If the report is valid, expect a fix or a mitigation plan within thirty days, and credit in the advisory unless you would rather not be named.

## What counts as a vulnerability here

`iris` runs decoders that came with the data. The whole point of the design is that the decoder is not trusted, so the interesting failures are the ones where an untrusted decoder or a malformed dataset reaches something it should not.

In scope, and taken seriously:

- A decoder escaping the WebAssembly sandbox, or reading or writing host memory it was not given.
- A decoder returning Arrow arrays that pass `iris-guard` and then cause out of bounds reads, uninitialised reads, or unsoundness downstream. This is the single most important class of bug in the project.
- A decoder consuming unbounded CPU, memory, or file descriptors despite the metering limits, or hanging a host thread rather than failing one query.
- A malformed dataset causing a panic, a crash, or memory unsafety in `iris-format`, `iris-source` or `iris-runtime`.
- A decoder reaching the network, the filesystem, the clock, or the environment through a host function it should not have been granted.
- Content addressing that can be defeated: a decoder whose digest matches the manifest but whose bytes do not, or a substitution path that runs native code for a digest it did not verify.
- Any path where a signature or digest check is skipped, cached incorrectly, or made non fatal by configuration that is not obviously named.

Out of scope:

- A decoder producing wrong but structurally valid output. That is a correctness bug, not a security bug. Report it as an issue.
- Denial of service through legitimately expensive work that stays inside the declared resource limits.
- Vulnerabilities in Wasmtime itself. Report those to the [Bytecode Alliance](https://github.com/bytecodealliance/wasmtime/security/policy), and tell us afterwards so we can bump the pin.
- Anything requiring the host to have already been compromised.

## Supported versions

Pre-alpha. There are no released versions yet, so only `main` is supported. This section will be replaced with a real table at the first release.

## Hardening notes

The trust boundary is documented in `docs/`, including what is inside the trusted computing base and what is not. If you find something inside it that should not be, that is a design bug and it is worth reporting even without an exploit.
