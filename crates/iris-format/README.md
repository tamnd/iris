# iris-format

Bundle and metadata format for iris self-decoding datasets.

Parses and writes the container that carries a dataset, the decoder reference, and the content digest that lets a host substitute a native decoder it already trusts.

The parser is split so that a host holding the whole file and a host reading through a window use the same code. `Directory` is the metadata on its own, parsed from a header and a footer somebody else fetched, and `Container` is that plus the payload behind it. There is one piece of code in this workspace that reads untrusted container metadata, and therefore one fuzz target that covers it.

A container is written either way too. `Builder::build` hands back the bytes and `Builder::build_into` writes them to anything that takes bytes, which is what makes a dataset larger than this host's address space possible to produce. The directory sits at the end of the file precisely so that nothing written early depends on anything decided late.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
