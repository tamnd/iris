#!/usr/bin/env python3
"""Reads a Parquet file with two implementations that know nothing about iris.

pyarrow is parquet-cpp and DuckDB has a Parquet reader of its own, so
between them this is two readers that share no code with each other and
none with the arrow-rs one `iris-parquet` is built on. That is the point:
the claim is that an unmodified reader is unaffected by the container in
the file, and an unmodified reader has to be somebody else's.

Nothing here is told where the container is or that there is one. These
are the calls anybody would make against any Parquet file.

The last part rewrites the file through pyarrow and reads the result,
which is how the documented behaviour gets checked: a rewrite produces a
new Parquet file, and a new Parquet file has no container in it and the
same rows.

Usage: ci/parquet-readers.py <parquet file>
"""

import sys

import duckdb
import pyarrow.parquet as pq

CONTAINER_KEY = b"iris.container"
DECODER_KEY = b"iris.decoder"


def total(table):
    """Every integer in the table added up, which is one number to compare."""
    return sum(sum(column.to_pylist()) for column in table.columns)


def main(path: str) -> int:
    table = pq.read_table(path)
    print(f"pyarrow_rows={table.num_rows}")
    print(f"pyarrow_columns={table.num_columns}")
    print(f"pyarrow_sum={total(table)}")

    # The keys are visible to a reader that does not act on them, which is
    # what makes this something somebody else could implement against.
    # They come off the file metadata and not off the Arrow schema: pyarrow
    # rebuilds the schema from the Parquet schema, so a key in the footer
    # that is not part of that never reaches `table.schema.metadata`. Worth
    # knowing, because reading the schema is the first place anybody looks.
    keys = pq.read_metadata(path).metadata or {}
    seen = CONTAINER_KEY in keys and DECODER_KEY in keys
    print(f"pyarrow_sees_keys={'yes' if seen else 'no'}")

    answer = duckdb.sql(
        f"select count(*) as rows, sum(c0 + c1 + c2 + c3) as total "
        f"from read_parquet('{path}')"
    ).fetchone()
    print(f"duckdb_rows={answer[0]}")
    print(f"duckdb_sum={answer[1]}")

    # The same two keys through the other reader, which has a table
    # function for exactly this and needed no extension to answer it.
    found = duckdb.sql(
        f"select count(*) from parquet_kv_metadata('{path}') "
        f"where key in ('iris.container', 'iris.decoder')"
    ).fetchone()
    print(f"duckdb_sees_keys={'yes' if found[0] == 2 else 'no'}")

    rewritten = path + ".rewritten"
    pq.write_table(table, rewritten)
    again = pq.read_table(rewritten)
    keys = pq.read_metadata(rewritten).metadata or {}
    carries = CONTAINER_KEY in keys
    print(f"rewrite_carries_container={'yes' if carries else 'no'}")
    print(f"rewrite_sum={total(again)}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
