# iris-trust

Decoder identity, content hashes and substitution policy.

A decoder is named by a URI and pinned by a BLAKE3 digest. A host that recognises the digest may run its own native implementation instead, and a host that does not may fetch and verify.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
