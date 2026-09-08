#!/usr/bin/env bash
# Writes a Parquet file with an iris container inside it and reads it back three ways.
#
# The claim this checks is the one #46 is about, and it has two halves that have to be checked
# separately because they are checked by different software.
#
# One half is that a reader that has never heard of iris reads the file and gets the rows, with no
# error, no warning and no special handling. That cannot be shown with the Parquet reader this
# workspace is built on, because a bug in an assumption about Parquet would be a bug both halves of
# such a test share. So it is shown with pyarrow, which is parquet-cpp, and with DuckDB, which has a
# Parquet reader of its own. Two implementations that share no code with ours and none with each
# other.
#
# The other half is that a reader that does know reads the same file through the decoder inside it
# and gets the same answers. That is `crates/iris-parquet/examples/decode.rs`, which opens no data
# page at all.
#
# The three sets of numbers are compared against each other rather than against constants written
# down here. A constant would be checked against the fixture and would then be a second copy of the
# fixture, and what matters is not that the sum is a particular number, it is that three readers
# that share nothing agree on it.
#
# Usage: ci/parquet-embed.sh
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

ROWS=1000
COLUMNS=4
WORK=target/parquet
CONTAINER=$WORK/sample.iris
PARQUET=$WORK/sample.parquet

# Windows runners have an interpreter called python and not one called python3.
PYTHON=python3
command -v python3 >/dev/null 2>&1 || PYTHON=python

mkdir -p "$WORK"
cargo run --release --locked -p iris-runtime --example write_container -- \
  "$CONTAINER" --rows "$ROWS" --columns "$COLUMNS"

echo "== writing a Parquet file with the container inside it =="
cargo run --release --locked -p iris-parquet --example embed -- "$CONTAINER" "$PARQUET" \
  | tee "$WORK/embed.txt"

echo "== reading it through the embedded decoder =="
cargo run --release --locked -p iris-parquet --example decode -- "$PARQUET" \
  | tee "$WORK/decode.txt"

echo "== reading it as plain Parquet, with two readers that know nothing about iris =="
"$PYTHON" ci/parquet-readers.py "$PARQUET" | tee "$WORK/readers.txt"

# One value out of one of the three files. Every number below is compared against another number
# rather than against a constant, so a fixture that changes moves all of them together.
value() {
  sed -n "s/^$2=//p" "$1"
}

check() {
  local what="$1" left="$2" right="$3"
  if [ "$left" != "$right" ]; then
    echo "$what: $left and $right do not agree" >&2
    exit 1
  fi
  echo "$what: $left"
}

check "rows, decoder and writer" "$(value "$WORK/decode.txt" rows)" "$(value "$WORK/embed.txt" rows)"
check "rows, decoder and pyarrow" "$(value "$WORK/decode.txt" rows)" "$(value "$WORK/readers.txt" pyarrow_rows)"
check "rows, decoder and duckdb" "$(value "$WORK/decode.txt" rows)" "$(value "$WORK/readers.txt" duckdb_rows)"
check "sum, decoder and pyarrow" "$(value "$WORK/decode.txt" sum)" "$(value "$WORK/readers.txt" pyarrow_sum)"
check "sum, decoder and duckdb" "$(value "$WORK/decode.txt" sum)" "$(value "$WORK/readers.txt" duckdb_sum)"
check "columns, decoder and pyarrow" "$(value "$WORK/decode.txt" columns)" "$(value "$WORK/readers.txt" pyarrow_columns)"

# The two keys are readable by a reader that does not act on them, which is what makes this a thing
# somebody else could implement against rather than a private arrangement.
check "pyarrow can see the pointer" "$(value "$WORK/readers.txt" pyarrow_sees_keys)" "yes"
check "duckdb can see the pointer" "$(value "$WORK/readers.txt" duckdb_sees_keys)" "yes"

# A file that has been through somebody else's writer is a plain Parquet file again. Checked
# because the documentation says so and a documented behaviour nobody checks is a hope.
check "a rewrite drops the container" "$(value "$WORK/readers.txt" rewrite_carries_container)" "no"
check "a rewrite keeps the rows" "$(value "$WORK/readers.txt" rewrite_sum)" "$(value "$WORK/decode.txt" sum)"

# The same rows written three ways, which is what `docs/PARQUET_PROPOSAL.md` argues from. The
# timings in that document came off a machine nobody else was using and are not asserted here,
# because a number that moves with the neighbours is not a thing to fail a build over. What is
# asserted is the part that does not move: where each of the three puts the container, and how much
# of the footer that costs. The footer is what every reader parses whether it wants the file or not.
echo "== the same rows written three ways =="
cargo run --release --locked -p iris-parquet --example variants -- "$CONTAINER" "$WORK/variants" \
  | tee "$WORK/variants.txt"
"$PYTHON" ci/parquet-cost.py "$WORK/variants" --repeats 3 | tee "$WORK/cost.txt"

check "the container in the gap is the container" \
  "$(value "$WORK/cost.txt" container)" "$(value "$WORK/embed.txt" container)"

# The pointer is two short strings and inlining is the whole container, base64, in the one part of
# the file nobody gets to skip. A hundred bytes of footer against several times the container.
POINTER=$(($(value "$WORK/cost.txt" footer_gap) - $(value "$WORK/cost.txt" footer_plain)))
if [ "$POINTER" -gt 1024 ]; then
  echo "the pointer costs the footer $POINTER bytes, which is more than it should" >&2
  exit 1
fi
echo "the pointer costs the footer: $POINTER bytes"

if [ "$(value "$WORK/cost.txt" footer_inline)" -le "$(value "$WORK/cost.txt" container)" ]; then
  echo "the inline footer is somehow no larger than the container it holds" >&2
  exit 1
fi
echo "inlining costs the footer: $(value "$WORK/cost.txt" footer_inline) bytes"

echo "PARQUET_EMBED_GREEN"
