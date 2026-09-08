# A Parquet file that carries the decoder it was written with

This is a proposal to the Apache Parquet community. It asks for no change to the format, no new encoding, and no new code in any reader. It describes a convention that works today in every Parquet reader that exists, says what it costs measured rather than asserted, and asks whether the community wants it named in the open rather than left as one vendor's arrangement.

The whole of it is one sentence. A writer may put arbitrary bytes between the last row group and the footer, where no offset names them, and say in the file metadata where they are, and a reader that has never heard of those bytes reads the file exactly as it always has.

## The problem is not the encodings

DELTA_BINARY_PACKED and DELTA_LENGTH_BYTE_ARRAY went into the format in 2015. BYTE_STREAM_SPLIT went in in 2019. All three are specified, all three are implemented in several readers, and a decade later a writer still cannot turn them on and expect the file to be readable.

The clearest statement of why is DuckDB's, in [Query Engines: Gatekeepers of the Parquet File Format](https://duckdb.org/2025/01/22/parquet-encodings) in January 2025, explaining why it implemented those encodings in 1.2.0 and does not write them by default: "If DuckDB did this, many of our users would have a frustrating experience because some mainstream query engines still do not support reading these encodings." The same post measures what is being given up by not writing them, on TPC-H: about 30 percent smaller with Snappy and about 15 percent faster to write, about 11 percent smaller with zstd and about 24 percent faster to write. On a column that suits the encoding, an integer sequence went from 3.7 GB to 1.3 MB.

That is not an argument about which encoding is best. It is a writer with the encoding implemented, the gain measured, and no way to take it, because the file has to be readable by whoever receives it and the writer does not know who that is. Every new encoding proposed to this format from now on inherits that position on the day it is accepted, and waits out the same decade.

## What is proposed

A file that answers the question itself. It is a Parquet file, with the rows in ordinary Parquet encodings that every reader already has, and it also carries the bytes of the decoder that the writer would rather have used, together with the data in that decoder's own encoding.

A reader that knows nothing reads the Parquet and is unaffected. A reader that knows about this reads the payload instead and gets whatever encoding the writer actually wanted, without anybody having agreed on that encoding in advance, without a spec change, and without a decade.

The layout is the only interesting part.

```text
+---------------------------------------------+ 0
| PAR1                                        |
+---------------------------------------------+ 4
| row groups, ordinary Parquet                |
+---------------------------------------------+ at
| the payload, whole                          |
+---------------------------------------------+ at + len
| the Parquet footer, which says where at is  |
+---------------------------------------------+
| footer length, PAR1                         |
+---------------------------------------------+
```

Between the last row group and the footer. A reader finds the footer from the last eight bytes and finds every column chunk from an offset in the footer, so bytes that no offset names are bytes it never asks for. The container needs no adjustment to anything else, because every offset in the footer was fixed before those bytes existed and the footer length is measured from the end of the file.

Two short strings in the file metadata say where it is and what is in it. In the implementation described below they are `iris.container`, holding the offset and the length as decimal numbers, and `iris.decoder`, holding a copy of what the payload says about its own decoder so that a reader can decide whether it cares while it still has only the footer in hand.

## This is not a new liberty

Parquet writers already put bytes in files that no offset names. [`parquet.writer.max-padding`](https://github.com/apache/parquet-java/blob/master/parquet-hadoop/README.md) in parquet-java defaults to 8 MB, and has since [PARQUET-321](https://issues.apache.org/jira/browse/PARQUET-321), so that a row group can be aligned to an HDFS block boundary. Files with unreferenced padding in them have been written by the reference implementation for years and every reader in the ecosystem ignores that padding without being told to.

What is proposed here is that padding with a purpose, and a footer key saying it is there. The reason to believe readers tolerate it is that they have been tolerating it since 2015.

## Why not the metadata, measured

Key and value metadata is where the format says extensions belong, and base64 into a value would work. It is worse, and this is the measurement the proposal rests on, because it decides whether the idea has the property that makes it interesting at all.

Every reader parses the whole footer to open a file, including the metadata it does not understand. A payload in there is a cost paid by exactly the readers that get nothing back for it, at the moment they can least afford it.

The same rows written three ways by the same writer, from `crates/iris-parquet/examples/variants.rs`. `plain` carries no payload. `gap` is the layout above. `inline` is the same payload base64 encoded into the file metadata. 200,000 rows of four `int64` columns, so the payload is 6,473,928 bytes. Apple M4, ten cores, 24 GiB, macOS 15.8, pyarrow 25.0.0 and DuckDB 1.5.1, forty repeats after five warm up passes, three rounds, cheapest round reported, median milliseconds.

| | plain | gap | inline | plain again |
|---|---|---|---|---|
| footer, bytes | 1,019 | 1,149 | 8,632,952 | |
| pyarrow, open | 0.041 | 0.049 | 6.705 | 0.046 |
| DuckDB, open | 0.222 | 0.378 | 3.147 | 0.228 |
| pyarrow, read | 3.495 | 3.522 | 12.126 | 3.998 |
| DuckDB, read | 2.143 | 2.609 | 8.084 | 2.622 |

The last column is the plain file measured again under another name, so that two of the numbers in every row are known to be the same file. Nothing separating the gap from the plain file is larger than what separates the plain file from itself, on either reader, on open or on a full read. Across three runs the gap file's DuckDB open landed 0.15 ms above the control once and below it the other two times, which is a machine rather than a file.

The inline column is not noise and does not need a careful reading. The footer goes from 1,149 bytes to 8,632,952, and opening the file goes from 0.05 ms to 6.7 on pyarrow and from 0.2 to 3.1 on DuckDB, for a reader that wanted the rows and will never touch the payload. That is the cost the gap does not have, and it is the entire reason the gap is worth arguing for.

## What it costs the writer

The rows are in the file twice, so the file is larger than either half.

| Fixture | Payload | Plain Parquet | With the payload in the gap | With it in the metadata |
|---|---|---|---|---|
| 1,000 rows, 4 columns | 105,928 | 38,298 | 144,353 | 179,566 |
| 200,000 rows, 4 columns | 6,473,928 | 7,477,903 | 13,951,961 | 16,109,836 |

Base64 costs a third on top of that, which is the smaller of the two reasons not to use it.

Whether the trade is worth taking is a question about a particular dataset and a particular encoding, and it is not one this proposal can answer for anybody. The fixture here is deliberately a poor advertisement: its decoder reads fixed width integers and compresses nothing, so the Parquet half is smaller than the payload half. A writer whose encoding is worth carrying is a writer for whom this arithmetic is different, and the DuckDB numbers above are one measurement of how far apart those two cases are.

## What the reader that does care pays

A decoder that arrives in a file has to be run, and running one is the part that deserves the most suspicion. In the implementation this comes from the decoder is a WebAssembly module, the host runs it in a sandbox with no syscalls and no imports beyond a narrow ABI, and the file commits to the decoder's digest in its own footer, so a host that trusts a particular decoder can recognise it and substitute a native implementation.

Two measurements matter for whether that is practical.

Opening one. Compiling the module dominates, at 30.41 ms median on the machine above, and a compilation cache keyed on the module digest and the engine fingerprint takes it to 0.13 ms, which is a saving of 99.6 percent. The first open after a deployment costs 2.4 ms more than it otherwise would, because it writes the artefact, and that is the only price.

Running one. On three decoders the sandbox costs about 1.7 times the instructions of the same decoder compiled natively, 1.71 on arm64 and 1.76 on x86-64, measured with one tool on comparable machines. What a core does with those extra instructions differs a great deal: an Apple M4 turns a 1.71 instruction ratio into a 1.29 duration ratio, and a server EPYC turns 1.76 into 1.83 cycles.

So a decoder in a file is somewhere between a quarter and twice as expensive as the same decoder built into the reader, and the reader gets an encoding that no reader has. Whether that is a good trade depends entirely on how much the encoding saves, which is the number the format's own history says nobody has been able to collect.

## What this is not

It is not a proposal to add an encoding to Parquet, and it does not compete with one. Every encoding worth standardising is still worth standardising, and this changes nothing about that argument except the cost of being early.

It is not a proposal to put WebAssembly, or any other decoder mechanism, into the Parquet specification. What is in the file is opaque bytes and two strings, and what a host does with them is entirely outside the format.

It is not a claim that anyone should read these files. A reader that ignores the two keys is a correct reader and stays correct forever.

## What does not work

A rewrite drops the payload. A reader that opens one of these files and writes it back out has written a new Parquet file, which has no gap in it and no keys. The rows survive because they were always ordinary Parquet. This is the honest behaviour rather than a limitation to work around, and it is checked in CI rather than asserted.

The hint in the metadata is not evidence. Anybody who can write the file can write anything in those two keys, so they are good for deciding whether to read further and useless for deciding whether to trust anything. The decision that matters, which is whether to run or substitute a decoder, is made against the payload's own commitments after reading it.

A tail prefetch reads some of the payload. Readers over object storage commonly fetch the last part of the file in one request to get the footer, and in one of these files some of that window is payload rather than row group data. It is the same number of bytes fetched in the same request and they are less useful ones. This has not been measured and is the largest thing on this page that has not.

The 8 MB of unreferenced padding parquet-java writes is a precedent for tolerating unreferenced bytes and not for tolerating megabytes of them. A payload that is a substantial fraction of the file is a bigger version of a thing readers already do, and whether any reader has an assumption that breaks at that size is a question for people who maintain readers, which is most of why this is being sent rather than merely written.

## What is being asked

Three things, none of which is a code change.

Whether the community agrees that a conforming reader never reads bytes that no offset in the footer names, and whether it is worth saying so in the specification. Everything above depends on it, every reader appears to implement it, and it appears to be written down nowhere.

Whether this should have names that are not a vendor's. The two keys in the implementation are `iris.container` and `iris.decoder` because they were written by one project, and a convention that two projects can use needs a name that belongs to neither. If the community wants such a name, this is an offer to write the note that defines it.

Whether anybody can see the failure mode nobody here has. The two readers this was checked against are pyarrow and DuckDB, which is two implementations sharing no code with each other and none with the arrow-rs one it was written on, and it is still only two.

## Where the numbers came from

Everything above is reproducible from a public repository, and none of it is a projection.

```
git clone https://github.com/tamnd/iris
cd iris
ci/parquet-embed.sh
```

That writes one file, reads it back three ways, and checks that the embedded decoder, pyarrow and DuckDB agree on the row count, the column count and the sum of every value, comparing them against each other rather than against constants. It runs on every pull request, so a change that breaks any of it is a red build rather than a surprise.

The timings come from `ci/parquet-cost.py`, on a machine nobody else was using, and are not asserted in CI because a number that moves with the neighbours is not something to fail a build over. The compilation cache numbers are in `docs/COLD_START.md` and the sandbox numbers are in `docs/VECTORISATION.md`, both with the machines and the method written down beside them. `docs/PARQUET.md` is the implementation guide.
