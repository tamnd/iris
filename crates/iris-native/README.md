# iris-native

Native implementations of decoders this host already knows, keyed by content hash.

A decoder ships inside the dataset and runs in a sandbox, which is what makes a container from anywhere readable. It also costs something, so a host that has its own implementation of a decoder it recognises should be able to run that instead and skip the sandbox. This is the table it is looked up in.

The key is the digest of the decoder module and there is nothing else in it. A table keyed on the decoder's name would hand native code, running in the host process with nothing around it, to any dataset that typed the right string, because the name in a container is chosen by whoever wrote the container. The digest used here is the one `iris-trust` computed from the module bytes that were actually present, so a decoder whose digest is unknown gets the WebAssembly path no matter what it calls itself.

Knowing which module an implementation stands in for says nothing about whether it stands in for it correctly, so the other half of this crate is proving that. A native implementation only reaches the table as a `Kernel`, and the only thing that makes a `Kernel` is a `Differential` run: both implementations over the same corpus, compared down to the byte, including how the rows were cut into batches. There is no other constructor, so a host that registers an unproved implementation does not compile. A run proves what its corpus covers and no more, which is what the `Corpus` documentation says in as many words.

The terms are part of what is proved. A kernel records the capability sets it was run under, and a host offering anything else gets the sandbox rather than code nobody compared under those terms.

Substitution replaces compiling and running the module and nothing else. The module is still hashed and checked, the handshake is still negotiated the same way, and every batch still goes through `iris-guard` and Arrow.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
