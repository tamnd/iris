#!/usr/bin/env bash
# Runs a real DuckDB session against a real container and checks the answers.
#
# This is what tests the extension. The crate has no test target because the symbols it calls do not
# exist until DuckDB loads it, so the only honest test is a database process reading a file, and that
# is what this is: build a container, build the extension, start `duckdb`, ask it questions.
#
# The five queries are not the same query five times. The first asks what the table looks like, which
# is the schema out of the container and nothing the extension was told. The second reads every
# column. The third reads two of the four and is the one that goes through projection pushdown. The
# fourth asks for a count, which projects no columns at all and is its own path through the scan. The
# fifth filters on a column, to check that a row came back whole rather than as four values that
# happen to be the right shape.
#
# `-unsigned` is needed because nobody signed this. DuckDB signs its own extensions and refuses
# unsigned ones unless a session says otherwise, which is the right default and is not something to
# work around anywhere but here.
#
# Usage: ci/duckdb-session.sh [path to the duckdb binary]
set -euo pipefail

DUCKDB="${1:-duckdb}"

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

ROWS=1000
COLUMNS=4
CONTAINER=target/duckdb/sample.iris
EXTENSION=target/duckdb/iris.duckdb_extension

mkdir -p target/duckdb
cargo run --release --locked -p iris-runtime --example write_container -- \
  "$CONTAINER" --rows "$ROWS" --columns "$COLUMNS"
ci/duckdb-extension.sh

# The platform the extension was built for has to be the platform the session runs on, and a
# mismatch there is the one failure whose message points at the wrong thing. Checked first.
SESSION_PLATFORM=$("$DUCKDB" -noheader -list -separator , -c "pragma platform")
BUILT_PLATFORM=$(python3 -c '
import sys
tail = open(sys.argv[1], "rb").read()[-534:]
print(tail[22 + 6 * 32 : 22 + 7 * 32].rstrip(b"\0").decode())
' "$EXTENSION")
if [ "$SESSION_PLATFORM" != "$BUILT_PLATFORM" ]; then
  echo "the extension is for $BUILT_PLATFORM and this duckdb is $SESSION_PLATFORM" >&2
  exit 1
fi

# Runs one query with the extension loaded and prints nothing but the answer.
ask() {
  "$DUCKDB" -unsigned -noheader -list -separator , -c "load '$EXTENSION'; $1"
}

# Checks an answer, and says both numbers when it is wrong rather than only that it was.
expect() {
  local what="$1" want="$2" got
  got=$(ask "$3")
  if [ "$got" != "$want" ]; then
    echo "$what: expected $want and got $got" >&2
    exit 1
  fi
  echo "$what: $got"
}

# The shape of the table, which is the schema out of the container and nothing DuckDB was told.
expect "columns" "c0,c1,c2,c3" \
  "select string_agg(column_name, ',') from (describe select * from iris_scan('$CONTAINER'))"

# Every column, every row. cell(column, row) is column * 1e9 + row, so this is the sum of the row
# indices plus a thousand of each column's offset.
expect "sum of everything" "6000001998000" \
  "select sum(c0 + c1 + c2 + c3) from iris_scan('$CONTAINER')"

# Two columns of the four, which is the projection the decoder is told about.
expect "two columns" "499500,2000000499500" \
  "select sum(c0), sum(c2) from iris_scan('$CONTAINER')"

# No columns at all, which is a count and is the path where the scan reads one column to find out
# how many rows there are and writes none of it out.
expect "count" "$ROWS" "select count(*) from iris_scan('$CONTAINER')"

# One row, whole. A read that came back holding somebody else's bytes shows up here as a value from
# the wrong row, which is what the fixture's values are built to make visible.
expect "one row" "42,1000000042,2000000042,3000000042" \
  "select c0, c1, c2, c3 from iris_scan('$CONTAINER') where c0 = 42"

echo "DUCKDB_SESSION_GREEN"
