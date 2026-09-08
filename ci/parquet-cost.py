#!/usr/bin/env python3
"""Times what an unmodified reader pays for a container it does not want.

`crates/iris-parquet/examples/variants.rs` writes the same rows three
ways: with no container, with the container in the gap between the last
row group and the footer, and with it base64 encoded into the file
metadata. This opens and reads all three with pyarrow and with DuckDB
and reports what each one cost.

The claim being measured is that the gap is free and the metadata is
not. It is a claim about readers that have never heard of iris, so it is
measured with readers nobody here wrote, and the reader is never told
which file is which.

Two timings per reader, because they answer different questions. An open
parses the footer and nothing else, which is what every reader does to
every file before it knows whether it wants any of it, and it is where a
container in the metadata lands. A read is the whole file, which is
where a container in the gap would land if it landed anywhere.

Every group of timings ends with a control, which is the plain file
measured a second time under another name. Two numbers in the group are
therefore the same file, and how far apart those two land is how far
apart two numbers can land here for no reason at all.

Usage: ci/parquet-cost.py <directory> [--repeats N]
"""

import base64
import gc
import statistics
import sys
import time

import duckdb
import pyarrow
import pyarrow.parquet as pq

CONTAINER_KEY = b"iris.container"
INLINE_KEY = b"iris.container.inline"

VARIANTS = ("plain", "gap", "inline")

DEFAULT_REPEATS = 25
WARMUPS = 5

# Every measurement is taken this many times over and the cheapest round
# is the one reported. The first file measured in a process was reliably
# a little slower than the same file measured later, whichever file it
# was, which is a fact about the interpreter and not about the file. A
# round is cheap and it removes that.
ROUNDS = 3


def timed(call, repeats):
    """Runs `call` and returns the median, the fastest and the slowest.

    Milliseconds. The garbage collector is off for the duration, because
    a collection that lands inside one sample is not a fact about the
    file that sample was reading.
    """
    rounds = []
    for _ in range(ROUNDS):
        for _ in range(WARMUPS):
            call()

        gc.disable()
        try:
            samples = []
            for _ in range(repeats):
                started = time.perf_counter()
                call()
                samples.append((time.perf_counter() - started) * 1000)
        finally:
            gc.enable()
        rounds.append(samples)

    samples = min(rounds, key=statistics.median)
    return statistics.median(samples), min(samples), max(samples)


def report(what, measured):
    """One measurement, as three numbers on one line."""
    median, fastest, slowest = measured
    print(f"{what}={median:.3f} {fastest:.3f} {slowest:.3f}")


def duckdb_open(connection, path):
    """The footer through DuckDB and nothing else."""
    connection.sql(f"select * from parquet_file_metadata('{path}')").fetchall()


def duckdb_read(connection, path):
    """Every row and every column through DuckDB.

    One connection does the whole run because `parquet_metadata_cache` is
    off by default, so every one of these parses the footer again rather
    than the first one parsing it and the rest reading a cache. Making a
    connection per sample would work too and would time the connection.
    """
    connection.sql(f"select * from read_parquet('{path}')").arrow()


def container_from_gap(path):
    """The container out of the gap file, read the way the pointer says."""
    at, length = pq.read_metadata(path).metadata[CONTAINER_KEY].split(b",")
    with open(path, "rb") as handle:
        handle.seek(int(at))
        return handle.read(int(length))


def main(directory: str, repeats: int) -> int:
    paths = {name: f"{directory}/{name}.parquet" for name in VARIANTS}

    # The two files are supposed to be carrying the same container by two
    # different means, and a cost comparison between them is worth
    # nothing if they are not. Python's own base64 decodes what the
    # example's encoder wrote, which is also the only check that encoder
    # gets.
    carried = container_from_gap(paths["gap"])
    inlined = base64.b64decode(pq.read_metadata(paths["inline"]).metadata[INLINE_KEY])
    if carried != inlined:
        print("the two files are not carrying the same container", file=sys.stderr)
        return 1
    print(f"container={len(carried)}")
    print(f"inline_encoded={len(pq.read_metadata(paths['inline']).metadata[INLINE_KEY])}")

    # The footer is the part of the file every reader parses whether it
    # wants any of the rest or not, so how big it is in each of the three
    # is most of the argument before a clock is involved at all.
    for name, path in paths.items():
        print(f"footer_{name}={pq.read_metadata(path).serialized_size}")

    connection = duckdb.connect()
    if connection.sql("select current_setting('parquet_metadata_cache')").fetchone()[0]:
        print("duckdb is caching parquet metadata, so an open cannot be timed", file=sys.stderr)
        return 1

    # The fourth measurement in each group is the control: the same file
    # as the first, measured again under another name. Two of these
    # numbers are the same file, so whatever separates them is what this
    # machine does to a number rather than what the files do, and no
    # difference smaller than that one means anything.
    measured = list(paths.items()) + [("control", paths["plain"])]

    for name, path in measured:
        report(f"pyarrow_open_{name}", timed(lambda p=path: pq.read_metadata(p), repeats))
    for name, path in measured:
        report(f"duckdb_open_{name}", timed(lambda p=path: duckdb_open(connection, p), repeats))
    for name, path in measured:
        report(f"pyarrow_read_{name}", timed(lambda p=path: pq.read_table(p), repeats))
    for name, path in measured:
        report(f"duckdb_read_{name}", timed(lambda p=path: duckdb_read(connection, p), repeats))
    connection.close()

    print(f"pyarrow={pyarrow.__version__}")
    print(f"duckdb={duckdb.__version__}")
    print(f"repeats={repeats}")
    return 0


if __name__ == "__main__":
    arguments = sys.argv[1:]
    count = DEFAULT_REPEATS
    if "--repeats" in arguments:
        index = arguments.index("--repeats")
        count = int(arguments[index + 1])
        del arguments[index : index + 2]
    if len(arguments) != 1:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(arguments[0], count))
