# iris-parquet

An iris container carried inside a Parquet file, where a reader that has never heard of iris does not have to do anything about it.

A file this crate writes is two things at once. It is a plain Parquet file, with the rows in ordinary Parquet encodings, which every Parquet reader in the world reads as it always has. It also carries a whole iris container, with the decoder inside it, and two keys in the file metadata that say where. A reader that knows those keys opens the container and gets the data through the decoder the writer chose. A reader that does not know them reads the Parquet and is not slowed down, warned, or broken by the rest.

That is the whole idea, and it is worth having because it asks nobody to adopt anything. A new format normally has to be read before it is worth writing and has to be written before it is worth reading, and most formats die in that circle. A file that is already a Parquet file is outside it.

`docs/PARQUET.md` is the guide.

The container sits between the last row group and the footer, and nothing in the footer points at it. A Parquet reader finds the footer from the end of the file and finds every column chunk by an offset the footer gives it, so those bytes are bytes it never asks for. The obvious alternative, base64 in the file metadata, is worse for a reason that matters: every reader parses the whole footer on open, so a container in there is a cost paid by exactly the readers that get nothing back for it.

What goes in the metadata is two short strings. `iris.container` is the offset and the length. `iris.decoder` is a copy of what the container says about its decoder, so a host can decide whether it wants anything to do with this file while it still has only the footer in hand. That copy is a hint and not evidence, and the crate documentation says why.

The crate is deliberately thin. It writes a Parquet file and reads a byte range back out of one, and it does not open a container, run a decoder or produce a batch. Keeping the runtime out of the dependencies is what lets a reader with no interest in iris depend on this to find out whether a file carries one.

`ci/parquet-embed.sh` is the test that matters. It writes one file and reads it three ways: through the embedded decoder, through pyarrow, and through DuckDB. The last two are implementations that share no code with each other and none with the arrow-rs reader this crate is built on, which is the only way to check a claim about what unmodified readers do.

Part of [iris](https://github.com/tamnd/iris). Licensed under Apache-2.0.
