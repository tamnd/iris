# iris-trust

Decoder identity, content hashes and substitution policy.

A decoder is named by a URI and pinned by a BLAKE3 digest. A host that recognises the digest may run its own native implementation instead, and a host that does not may fetch and verify.

What is here now is the check that has to come before any of that is worth having. `decoder` takes a container, hashes the module in it, compares the hash to what the container says, and hands the bytes over only if the two agree. It is the only way to get the module, so a host cannot compile one it has not verified, and there is no setting that changes that.

```rust
let container = iris_format::Container::parse(&bytes)?;
let decoder = iris_trust::decoder(&container)?;
// `decoder.module()` is the same bytes that were hashed.
```

The container's root digest covers the header and the footer, which is what makes a file cheap to open. A byte changed inside the decoder section therefore parses without complaint, and this is what catches it.

Substitution, signatures and a policy about what may be fetched are still ahead.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
