#!/usr/bin/env python3
"""Appends the metadata block that lets DuckDB load a shared library.

DuckDB will not load an extension that does not end in one of these, and the
error when it is missing or wrong says the file is not a valid extension rather
than saying what about it is invalid. So the block is written here, in one
place, from a description of what is in it.

The layout, which is stable and is what a real extension on this machine was
read to confirm:

    22 bytes    a WebAssembly custom section header naming the section
                duckdb_signature, which is what lets the same trailer work for
                a native library and for a Wasm module
    8 x 32      eight fields, ASCII, padded with nul bytes, written from the
                last one to the first
    256 bytes   the signature, all zero for an extension nobody signed

Of the eight fields the first three written are empty and are room DuckDB left
itself. The rest, in the order they are written, are the ABI type, the version
of the extension, the version of DuckDB it was built against, the platform, and
the metadata version, which is 4.

For an extension built against the C API the ABI type is C_STRUCT and the DuckDB
version field holds the minimum C API version the entry point asked for rather
than a release of DuckDB, because a C API extension is not tied to one.

Usage: ci/duckdb-metadata.py <library> <output> --extension-version V
                             --duckdb-version V --platform P [--abi-type T]
"""

import argparse
import pathlib
import sys

# The custom section header, byte for byte. The name it carries is
# duckdb_signature and the two bytes after it open the payload.
PREFIX = bytes([0, 147, 4, 16]) + b"duckdb_signature" + bytes([128, 4])

FIELD_WIDTH = 32
SIGNATURE_WIDTH = 256


def field(value: str) -> bytes:
    """One field, as ASCII padded to its width."""
    encoded = value.encode("ascii")
    if len(encoded) > FIELD_WIDTH:
        raise SystemExit(f"{value!r} does not fit in {FIELD_WIDTH} bytes")
    return encoded.ljust(FIELD_WIDTH, b"\0")


def trailer(abi_type: str, extension_version: str, duckdb_version: str, platform: str) -> bytes:
    """The whole block, ready to be put on the end of a library."""
    fields = ["", "", "", abi_type, extension_version, duckdb_version, platform, "4"]
    return PREFIX + b"".join(field(value) for value in fields) + bytes(SIGNATURE_WIDTH)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--extension-version", required=True)
    parser.add_argument("--duckdb-version", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--abi-type", default="C_STRUCT")
    args = parser.parse_args()

    body = args.library.read_bytes()
    # Written whole rather than appended in place, so that a second run over a
    # file that already has a trailer does not produce one with two.
    args.output.write_bytes(
        body
        + trailer(
            args.abi_type,
            args.extension_version,
            args.duckdb_version,
            args.platform,
        )
    )
    print(f"{args.output} is {args.library} plus {len(PREFIX) + 8 * FIELD_WIDTH + SIGNATURE_WIDTH} bytes of metadata")
    return 0


if __name__ == "__main__":
    sys.exit(main())
