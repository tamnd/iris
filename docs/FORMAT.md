# The container format

This describes the file that carries an iris dataset. It is what `iris-format` reads and writes, and it is a different thing from the ABI in `docs/ABI.md`. The ABI is how a host and a decoder talk to each other while a query is running. The container is what sits on disk or in object storage before any of that starts.

The two are related in one place, which is that the footer reuses the record framing from `iris-abi`. That reuse is deliberate and it is explained below.

## Shape

```text
+--------------------------------------------------+ 0
| header, 16 bytes                                 |
+--------------------------------------------------+ 16
| sections, back to back, each 8 byte aligned      |
+--------------------------------------------------+ footer_offset
| footer, a run of records                         |
+--------------------------------------------------+ len - 56
| trailer, 56 bytes                                |
+--------------------------------------------------+ len
```

The directory is at the end rather than the front. A writer that streams a large dataset does not know how long a section is until it has finished writing it, and the alternatives are buffering the whole dataset in memory or seeking backwards over it, neither of which is available when the destination is an object store. Putting the directory last is what Parquet does and for the same reason.

The magic appears at both ends. That means a file truncated in the middle is distinguishable from a file that was never an iris container, which is worth the eight bytes: those two problems have completely different causes and telling somebody the wrong one sends them looking in the wrong place.

## Header

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | magic, `49 52 49 53 0d 0a 1a 0a` |
| 8 | 2 | format major |
| 10 | 2 | format minor |
| 12 | 4 | flags, reserved, must be zero |

The magic spells `IRIS` followed by a carriage return, a line feed, a `0x1a` and another line feed. The carriage return and line feed catch a transport that helpfully converts line endings. The `0x1a` stops a Windows `type` from printing the rest of the file. Both of those are older than most of the people who will read this and both still happen.

`flags` is reserved and a reader refuses a container where it is not zero. That is the point of a reserved field: it is the one place a future version can put a change that an older reader must not ignore. Everything else in the format is arranged so that an older reader can carry on, so there has to be exactly one lever that says stop.

## Trailer

| Offset from the end | Size | Field |
| --- | --- | --- |
| 56 | 8 | footer offset |
| 48 | 4 | footer length |
| 44 | 4 | reserved, must be zero |
| 40 | 32 | root digest |
| 8 | 8 | magic |

The footer length is a `u32`. The footer describes a dataset, it is not the dataset, and four gigabytes of metadata means something has gone wrong that a wider field would not fix.

## Sections

A section is a run of bytes in the payload area with an identifier, a kind and a digest. Sections start on an eight byte boundary so that a decoder can point a wide load at one without a misaligned access.

Kinds are `data` for encoded column data, `decoder` for an embedded decoder module, and `sidecar` for anything the decoder wants to find without reading the data, such as an index or a dictionary. A reader that meets a kind it does not know still gets a run of bytes with a digest, so it can still check the file and still copy it.

Section bounds are checked at parse time against the payload area, which runs from the end of the header to the start of the footer. A section may not overlap the header, the footer or the trailer, and the addition of its offset and its length is checked for overflow, because the first thing a hostile file does is pick two numbers that wrap round to something small and reasonable looking.

## Footer

The footer is a run of records using the framing from `iris-abi`: a two byte tag, a two byte version, a four byte length, and a payload padded to a multiple of eight.

Sharing the framing means the rule about unknown records is the one that is already written down and already tested, rather than a second implementation of the same idea that drifts from the first. The two do not share a tag number space, because a footer record and a call record never appear in the same byte stream and there is nothing to collide.

| Tag | Record | How many |
| --- | --- | --- |
| 0x0100 | dataset | exactly one |
| 0x0101 | schema | zero or one |
| 0x0102 | decoder | zero or one |
| 0x0103 | section | any number |

**dataset** carries the row count as a `u64` and a name. The name is for error messages and for `iris describe`. Nothing looks it up.

**schema** carries an encoding code and the schema bytes. This crate does not depend on Arrow and does not look inside those bytes. Carrying the encoding rather than assuming it means a second one can be added later without a new major version, and it keeps the crate small enough to fuzz seriously.

**decoder** carries the ABI version the decoder was built against, where the module is, the digest of the module, the capabilities the decoder needs from the host, and a name. Having the required capabilities here as well as in the handshake means a host can refuse a dataset before it loads a single instruction of it, which is a much better error than one that arrives halfway through a query.

The digest is the identity of the decoder. A host that already trusts a native implementation of that exact module substitutes it and skips the sandbox entirely, and a host that does not runs the bytes it hashed. Either way there is nothing to guess about which decoder the dataset asked for.

**section** carries an identifier, a kind, an offset, a length and a digest.

## Digests

Each section carries the digest of its bytes. The footer carries the section records. The trailer carries a digest over the header and the footer. So the trailer commits to the footer and the footer commits to every section, which is enough to say two files are the same dataset without comparing them byte for byte.

BLAKE3 rather than SHA-256, because this runs over whole datasets on the write path and over whole sections on any read that verifies, and the difference is large enough to change whether verification is on by default.

Parsing checks the root digest, because the footer is small and it makes a parsed container mean the metadata is what the writer wrote. Verifying the sections is a separate call, because it reads the whole file. Opening a hundred gigabyte dataset should not hash a hundred gigabytes. The honest place for a full verify is once, when a dataset arrives.

## Parsing is the untrusted path

A dataset comes from somewhere. Reading one must not panic, must not read out of bounds, and must not allocate on the basis of a length field that has not been checked.

The crate forbids `unsafe` outright, so out of bounds is a language guarantee rather than a promise anybody has to keep. The other two are properties of how the parser is written:

- no arithmetic on a length from the file that is not checked or saturating
- nothing allocated in proportion to a number read out of the file, only in proportion to bytes that are actually there

The second one is the one that gets skipped in hand written parsers, and it is the one that turns a sixty byte file into an out of memory kill. The format is arranged to make it hard to get wrong: there is no count field anywhere in the footer. The number of sections is however many section records the footer actually contains, so a file that claims a billion sections has to be large enough to hold a billion section records.

There is a test that truncates a good container at every possible length, a test that flips a bit in every byte of the metadata, and a fuzz target that runs nightly.

## What may change without a major version

The same three rules as the ABI, for the same reasons:

1. A footer record may grow a field at the end. A reader that does not know about the field reads what it knows and steps over the rest.
2. A new footer tag may be added. A reader that does not know the tag steps over the record.
3. A new section kind, schema encoding or decoder location may be added. Each of those decodes to an `Unknown` variant that carries the number, so a reader can say what it found rather than refusing to open the file.

Anything else is a major version, and a reader refuses a major version it does not know rather than trying its luck.

A known record at a version higher than the reader understands is refused rather than skipped. An unknown tag means the reader has never heard of this, which is safe to ignore. A known tag at a higher version means the fields are in the same places but no longer mean the same things, and reading them anyway is how a parser produces a confident wrong answer.

## What is not decided yet

- **Encryption.** There is no encryption in the format. When it arrives it will be per section, because that is the only granularity at which the range serving in the ABI still works, and the key management is somebody else's problem.
- **Compression of the footer.** Not worth it at the sizes seen so far. If a dataset ever has enough sections for it to matter, that probably says the sections are too small.
- **Multi file datasets.** A container is one file today. A dataset spread over many files needs either an index container or a naming convention, and there is no reason to pick yet.
- **Statistics.** Nothing in the footer carries minimum, maximum or null counts per column. That belongs in a sidecar section for now, because putting it in the footer freezes a statistics model before anybody has used one.
