# A decoder inside a Parquet file

```
cargo run --release -p iris-parquet --example embed -- sample.iris sample.parquet
```

What comes out is a Parquet file. Every Parquet reader in the world reads it, gets the rows, and says nothing about the rest. It also carries the whole iris container the rows came from, decoder included, and two keys in the file metadata saying where that is. A reader that knows about those keys reads the file through the decoder instead.

This is the one item in the adoption plan that asks nobody to adopt anything. A format normally has to be readable before anybody writes it and has to be written before anybody bothers reading it, and most formats die inside that circle. A file that is already a Parquet file starts outside it: it can be published today, to readers that exist today, and the decoder is there for whoever eventually wants it.

## Where the container goes

```text
+---------------------------------------------+ 0
| PAR1                                        |
+---------------------------------------------+ 4
| row groups, ordinary Parquet                |
+---------------------------------------------+ at
| the iris container, whole                   |
+---------------------------------------------+ at + len
| the Parquet footer, which says where at is  |
+---------------------------------------------+
| footer length, PAR1                         |
+---------------------------------------------+
```

Between the last row group and the footer, and nothing in the footer points at it.

That position is not a trick, it follows from how Parquet is read. A reader finds the footer by reading the last eight bytes, and finds every column chunk by an offset the footer gives it. Bytes that no offset names are bytes nobody asks for. Writing the container there needs no adjustment to anything else: every offset in the footer was fixed before those bytes existed, and the footer length is measured from the end of the file.

It also needs no new code in the Parquet writer. `arrow-rs` closes a row group when asked, says how many bytes it has written, and will write bytes through its own counter, so the whole of it is: write the batches, close the row group, note the offset, write the container, add two keys, write the footer.

## Why not base64 in the metadata

Parquet's key and value metadata is where extensions are supposed to live, and a container base64 encoded into a value would work. It is worse in the way that matters most here.

Every Parquet reader parses the whole footer when it opens a file, including metadata it does not understand. A container in there is a cost paid by exactly the readers that get nothing back for it, and paid on open rather than on scan, which is the moment a reader can least afford it. It is also a third larger than it needs to be. The gap costs an unmodified reader nothing at all, and that property is the entire argument, so it is not one to trade away for tidiness.

What does go in the metadata is two short strings.

| Key | Value |
| --- | --- |
| `iris.container` | the offset and the length, in decimal, with a comma between them |
| `iris.decoder` | the ABI version, the digest of the module and the name, with commas between them |

## The hint is a hint

`iris.decoder` is a copy of what the container's own footer says. It is there so a host can decide whether it wants anything to do with this file while it still has only the Parquet footer in hand, rather than after a second read. Deciding to read further is a decision that is safe to get wrong.

It is not evidence and must not be used as any. The container commits to its own footer with a digest and these two keys are outside that, so anybody who can write the file can write whatever they like in them. The decision that is not safe to get wrong is whether to substitute a trusted native implementation for the module in the file, and that one is made against the container's own decoder record after opening it, by the host, exactly as it is for a container that arrived on its own.

## Both directions, checked against other people's readers

`ci/parquet-embed.sh` writes one file and reads it three ways.

The first is `crates/iris-parquet/examples/decode.rs`, which opens no data page at all. It reads the footer, learns from two keys that this file carries a container, reads that range and nothing else, and runs the decoder inside it.

The second and third are pyarrow and DuckDB. That is deliberate and it is the only part of this that could not be done with a unit test. The claim is about what readers that have never heard of iris do, and the Parquet reader this workspace is built on shares its assumptions with the writer, so a test using it would agree with itself. pyarrow is parquet-cpp and DuckDB has a reader of its own, so they share no code with each other and none with arrow-rs.

All three agree on the row count, the column count and the sum of every value, and the script compares them against each other rather than against constants written down in it, because what matters is not that the sum is a particular number.

Both readers can also see the two keys without being taught anything. In DuckDB it is `select * from parquet_kv_metadata('sample.parquet')`. In pyarrow it is `pq.read_metadata(path).metadata`, and not `table.schema.metadata`, which is the first place anybody looks and does not have them: pyarrow rebuilds the Arrow schema from the Parquet schema, so a footer key that is not part of that never reaches it.

## What it costs

The fixture is a thousand rows of four integer columns. The container is 105,928 bytes, and the Parquet file that carries it is 144,353. So the rows are in the file twice and the file is about a third larger than the container alone.

The obvious question is which half is smaller, and for this fixture it is Parquet, comfortably. That says nothing about iris and everything about the fixture: the decoder in it is `crates/iris-decoder/examples/fixedwidth.rs`, which reads fixed width integers and compresses nothing, against a Parquet writer doing dictionary encoding and run length encoding as a matter of course. The comparison worth making is in `docs/VECTORISATION.md` and in `tamnd/iris-bench`, against a decoder that is trying.

What this page is claiming is the mechanism and not a ratio. A dataset whose encoding beats Parquet by enough to be worth carrying can now be carried in a Parquet file, and it is the same file either way.

## What does not survive

Nothing about this survives a rewrite. A reader that opens one of these files and writes it back out has written a new Parquet file, and a new Parquet file has no gap and no keys in it. The rows survive, because they were always there as ordinary Parquet.

That is the honest behaviour rather than a limitation to work around. The container was in that file because a writer put it there, and a writer that does not know about iris has not put one anywhere. `ci/parquet-embed.sh` rewrites the file through pyarrow and checks both halves of that: the container is gone and the rows are unchanged.

## How big it is

The crate is 332 lines, of which 158 are code. With the two examples, the tests and the script that drives the other readers, everything this is made of comes to 939 lines.

## What is not here

No reader for other people's Parquet. `iris-parquet` finds a container in a file or says there is not one, and reading the Parquet is the job of whichever Parquet reader the caller already has.

No writer that produces the container. The container comes from wherever containers come from and this crate puts it in a file. The example uses `iris-runtime` to read one back so it has batches to write, and that is a dev-dependency, so nothing that depends on this crate pulls a wasm engine in behind it.

No filter or projection pushdown into the embedded path beyond what the container itself does. A reader that opens the container gets whatever the decoder offers, which for projection is real and for filters is not.
