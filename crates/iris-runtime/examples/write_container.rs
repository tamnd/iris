//! Writes a container to a file, for something outside this workspace to open.
//!
//! Every test in the tree builds its fixture in memory and never writes one down, which is right for
//! a test and useless to a C program on a machine that has no Rust on it. The clean machine gates in
//! `.github/workflows/release.yml` need a real file to hand to `crates/iris-c/examples/scan.c` and to
//! the Python tests, and this is what produces it.
//!
//! ```text
//! cargo run --release -p iris-runtime --example write_container -- sample.iris
//! ```
//!
//! `--rows` and `--columns` move the fixture. The decoder inside it is
//! `crates/iris-decoder/examples/fixedwidth.rs`, compiled for wasm32 on the way in, which is the same
//! decoder every gate test drives. Nothing about the container is special: it is a container, and the
//! point of the exercise is that a reader needs nothing but the file.

use std::env;
use std::fs;
use std::process::ExitCode;

#[path = "../tests/support/mod.rs"]
mod support;

use support::builder;

/// Rows in the fixture unless `--rows` says otherwise.
const DEFAULT_ROWS: u64 = 1_000;

/// Columns in the fixture unless `--columns` says otherwise.
const DEFAULT_COLUMNS: u64 = 4;

fn main() -> ExitCode {
    let mut path = None;
    let mut rows = DEFAULT_ROWS;
    let mut columns = DEFAULT_COLUMNS;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rows" => rows = parse(args.next().as_deref(), "--rows"),
            "--columns" => columns = parse(args.next().as_deref(), "--columns"),
            other => path = Some(other.to_owned()),
        }
    }

    let Some(path) = path else {
        eprintln!("usage: write_container <path> [--rows N] [--columns N]");
        return ExitCode::FAILURE;
    };

    let bytes = builder(rows, columns)
        .build()
        .expect("a container this size fits");
    let size = bytes.len();

    match fs::write(&path, bytes) {
        Ok(()) => {
            println!("{path}: {rows} rows, {columns} columns, {size} bytes");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{path}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Reads a count, or stops. A fixture built from a number nobody meant is worse than no fixture.
fn parse(value: Option<&str>, flag: &str) -> u64 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{flag} wants a number"))
}
