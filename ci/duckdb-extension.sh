#!/usr/bin/env bash
# Builds the DuckDB extension and puts the metadata block on the end of it.
#
# A `.duckdb_extension` file is a shared library with a trailer, so this is two steps: cargo builds
# the cdylib and ci/duckdb-metadata.py writes the trailer. Neither step needs DuckDB installed. The
# crate is built against the C extension API rather than against a database, which is the whole
# reason a release can produce one of these for five platforms from machines that have no DuckDB on
# any of them.
#
# The result lands in target/duckdb/iris.duckdb_extension. The name matters: DuckDB takes the
# extension name from the file name and then looks for an entry point called iris_init_c_api, so a
# file called anything else loads and then fails to find its entry point.
#
# Usage: ci/duckdb-extension.sh [--target <triple>]
set -euo pipefail

TARGET=""
if [ "${1:-}" = "--target" ]; then
  TARGET="${2:?usage: ci/duckdb-extension.sh [--target <triple>]}"
fi

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

# The host triple when nobody named one, so the mapping below has something to work on either way.
TRIPLE="${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"

# DuckDB's own name for the platform, which is what goes in the metadata and what a session compares
# against before it will load anything. The five here are the five a release builds.
case "$TRIPLE" in
  aarch64-apple-darwin) PLATFORM=osx_arm64 ;;
  x86_64-apple-darwin) PLATFORM=osx_amd64 ;;
  aarch64-unknown-linux-gnu) PLATFORM=linux_arm64 ;;
  x86_64-unknown-linux-gnu) PLATFORM=linux_amd64 ;;
  x86_64-pc-windows-msvc) PLATFORM=windows_amd64 ;;
  *)
    echo "no DuckDB platform name for $TRIPLE" >&2
    exit 1
    ;;
esac

case "$TRIPLE" in
  *-apple-*) LIBRARY=libiris_duckdb.dylib ;;
  *-windows-*) LIBRARY=iris_duckdb.dll ;;
  *) LIBRARY=libiris_duckdb.so ;;
esac

# Windows runners have an interpreter called python and not one called python3, and this script runs
# on all five platforms because the extension is built for all five.
PYTHON=python3
command -v python3 >/dev/null 2>&1 || PYTHON=python

VERSION=$(cargo metadata --format-version 1 --no-deps \
  | "$PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

# Read out of the source rather than written down again here. It is an argument to the macro that
# generates the entry point, so the number the library was actually built with is the one in that
# file, and a copy of it in this script would be a second answer that nothing checks.
MIN_DUCKDB_VERSION=$(sed -n 's/.*min_duckdb_version = "\([^"]*\)".*/\1/p' crates/iris-duckdb/src/lib.rs)
if [ -z "$MIN_DUCKDB_VERSION" ]; then
  echo "could not find min_duckdb_version in crates/iris-duckdb/src/lib.rs" >&2
  exit 1
fi

if [ -n "$TARGET" ]; then
  cargo build -p iris-duckdb --release --locked --target "$TARGET"
  BUILT="target/$TARGET/release/$LIBRARY"
else
  cargo build -p iris-duckdb --release --locked
  BUILT="target/release/$LIBRARY"
fi

mkdir -p target/duckdb
"$PYTHON" ci/duckdb-metadata.py "$BUILT" target/duckdb/iris.duckdb_extension \
  --extension-version "v$VERSION" \
  --duckdb-version "$MIN_DUCKDB_VERSION" \
  --platform "$PLATFORM"
