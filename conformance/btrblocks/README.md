# The BtrBlocks conformance corpus

M5 asks for a BtrBlocks decoder that is byte identical to the reference implementation, and says that any deviation is a bug in ours until proven otherwise. That is a claim about agreement with somebody else's code, so it cannot be checked against a test we wrote. It has to be checked against output the reference itself produced.

This directory is how that output gets here. `generate.cpp` links against the reference, compresses a set of columns with it, reads them back with it, and writes down both the compressed bytes and the values the reference got out of them. `fixtures/` is what it produced. The Rust tests in `crates/iris-btr` read the same compressed bytes and must produce the same values, byte for byte.

## The reference

github.com/maxi-k/btrblocks, MIT licensed, at commit `c950c3c01c1fbf91007936b05781d431a29a60d3`.

Pinned to a commit because the project has no tags at all. There is no release to name, so the alternative to a commit is a branch, and a branch would mean the corpus was generated against something nobody can identify later.

## Building and running the generator

Nothing in CI does this. The fixtures are committed and the tests read them from disk, so a change to iris needs no C++ toolchain and no copy of the reference. Regenerating is a deliberate act, done when the corpus needs to grow or when the pin moves.

Build the reference first, in place, since the generator links against the static library its build leaves behind:

    git clone https://github.com/maxi-k/btrblocks
    cd btrblocks
    git checkout c950c3c01c1fbf91007936b05781d431a29a60d3
    mkdir build && cd build
    cmake -DCMAKE_BUILD_TYPE=Release ..
    make -j8 btrblocks

Then the generator, pointed at that checkout:

    mkdir build && cd build
    cmake -DCMAKE_BUILD_TYPE=Release -DBTRBLOCKS_ROOT=/path/to/btrblocks ..
    make
    ./generate ../fixtures c950c3c01c1fbf91007936b05781d431a29a60d3

Linux only, and x86-64 or arm64. The reference says so about itself, and its build compiles with `-march=native`, so the generator has to as well or a header inlined at one instruction set ends up calling into an object built for another.

## What a case is

Three files per case, named after it.

`<case>.btr` is a column part exactly as the reference writes one: a `ColumnPartMetadata` header of a chunk count and one offset per chunk, followed by the compressed chunks. This is the unit `ColumnPart::writeToDisk` produces and the unit `BtrReader` consumes, so it is the smallest thing that can be handed to a decoder without inventing a container around it. Every case in this corpus holds one chunk.

`<case>.out` is what the reference decoded, in a canonical form defined by the generator:

- an integer column is `tuple_count` little endian `int32`
- a double column is `tuple_count` little endian `float64`
- a string column is `tuple_count + 1` little endian `uint32` offsets followed by the bytes they point into, with each offset counted from the start of the offset array

`<case>.null` is `tuple_count` bytes, one per row, 1 where the row is present and 0 where it is null.

The canonical form is not the buffer the reference's decompressor filled, and the difference matters. A string column can come back either as bytes or as a table of pointers into the compressed input, depending on the scheme, and a table of pointers is not comparable between two processes. The slots belonging to null rows come back holding whatever was in the allocation, so a byte comparison over them would be a comparison of the allocator. The generator reads strings through whichever viewer the reference says applies, writes them out one way, and zeroes the value slot of every null row.

## What the corpus covers

Twenty two cases, 8192 rows each.

Sixteen of them name the scheme to compress with rather than letting the reference choose. Choosing is done by sampling the column, so a corpus that let the reference decide would cover whichever schemes it happened to prefer on this data, and would quietly stop covering one the day the sampling changed its mind. Bit packing is the clearest example: it is a scheme every reader has to implement, and on data shaped to suit it the reference still prefers something else often enough that leaving it to chance is not good enough.

Only the top level is forced. Whatever a scheme puts underneath itself, the codes of a dictionary or the offsets of an FSST column, is chosen normally, so the cascades in the corpus are the ones the reference would really produce. `manifest.txt` records the full cascade for each case, which is how a case that stopped being what its name says shows up as a diff.

Three more cases have a scattered nullmap, one per column type, which is where a decoder that reads the values and the nullmap independently of each other gets caught. Three have no present rows at all. The reference short circuits an empty column to `ONE_VALUE` before it looks at any forced scheme, so those three are named for their shape rather than for a scheme.

The generator checks every case round trips before it writes it. Nothing in the reference stops a scheme being forced onto data it was not meant for, and a scheme that mangled such a column would still produce a fixture, which would then be committed as the answer our reader is graded against. That is the one way this program could be confidently and silently wrong, so it is the one thing it checks.

## Compression here is not reproducible

Running the generator twice on the same input does not produce the same corpus, and this is a property of the reference rather than of anything here.

The reference picks a scheme by sampling the column, and seeds that sample from `std::random_device`. Two runs can therefore disagree about which scheme to use. Forcing the scheme removes that at the top level, and it is still there for every cascade underneath.

Beyond that, the two FSST cases differ between runs even with the scheme forced. The compressed part comes out the same length and three bytes of it change, in the region holding the symbol table. What the reference decodes from either version is identical, so this is not a disagreement about the data.

The consequence is the reason the corpus is committed rather than regenerated as part of a build. These bytes are the artifact. Regenerating produces an equivalent corpus and not the same one, so a regeneration is reviewed as a change like any other rather than expected to be a no-op.

There is a way to make selection deterministic, which is to ask the reference to try every scheme and keep the smallest output. It is not used here, because it also takes the choice away and hands it to whichever scheme compresses best, which on this data is never bit packing. A corpus that has to be committed is a smaller problem than a corpus that cannot cover a scheme.
