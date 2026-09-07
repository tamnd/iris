//! What opening a container costs, and what a compilation cache takes off that.
//!
//! The remote scan report named this as the single largest addressable item in its table: about
//! thirty milliseconds of every open went on hashing the decoder section and compiling it to machine
//! code, and there was nowhere to keep the result, so every process paid it again. This measures the
//! same open four ways, so that what the cache is worth is a number rather than a claim.
//!
//! Run it with `cargo run --release -p iris-runtime --example cold_start`. Add `--json` for a machine
//! readable object suitable for handing to iris-bench.
//!
//! # The four ways
//!
//! `no cache` is what a host paid before any of this existed. A fresh runtime opens the container,
//! compiles the decoder and stops, and the next repeat starts again from nothing. This is the number
//! a process pays on every start.
//!
//! `cold` is a fresh runtime against an empty directory. It compiles, it serialises what it compiled,
//! it writes that, and it loads it back. So this is the first start after a deployment, and it is
//! deliberately reported rather than folded away, because it is more work than `no cache` and
//! somebody deciding whether to turn the cache on should see what the first open costs them.
//!
//! `warm` is a fresh runtime against a directory that already has the artefact. This is every start
//! after the first, and it is the number the whole thing is for.
//!
//! `pooled` is the same runtime opening the container a second time, so the decoder is already
//! compiled in this process and nothing is looked up at all. It is the floor. It says how much of
//! what is left in `warm` is the container rather than the decoder, and there is no arrangement of
//! caches that gets an open below it.
//!
//! # What is not being measured
//!
//! Scanning. Every shape here opens the container and stops, because folding a scan into the number
//! would hide the thing being measured under the rows. What a host feels is this cost divided by the
//! work of the query that followed it, and a host running one small query per process feels all of
//! it.
//!
//! The container is resident, so nothing here touches a network or a disk except the cache itself.
//! An open over object storage pays the round trips in the remote scan report on top of everything
//! here, and those numbers are unaffected by any of this.

use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use iris_runtime::Runtime;

#[path = "../tests/support/mod.rs"]
mod support;

use support::builder;

/// Rows in the fixture unless `--rows` says otherwise.
///
/// Small on purpose. What is being measured is a fixed cost per open, and a container large enough
/// to matter would put bytes in the way of a number that does not depend on them. The decoder
/// compiled here is the same decoder however many rows it is going to read.
const DEFAULT_ROWS: u64 = 1_000;

/// Columns in the fixture.
const COLUMNS: u64 = 4;

/// How many times each shape is opened unless `--repeats` says otherwise.
const DEFAULT_REPEATS: usize = 25;

/// A summarised sample set, in milliseconds.
struct Summary {
    median: f64,
    lo: f64,
    hi: f64,
    n: usize,
}

fn median_of(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    }
}

/// Median, with the fastest and the slowest beside it.
///
/// Not a bootstrap interval, unlike the guard cost probe. Each sample here is tens of milliseconds
/// and there are a few dozen of them, so the honest thing to show is the spread that was actually
/// observed rather than an interval resampled out of it.
fn summarise(samples: &[f64]) -> Summary {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    Summary {
        median: median_of(&sorted),
        lo: sorted.first().copied().unwrap_or(f64::NAN),
        hi: sorted.last().copied().unwrap_or(f64::NAN),
        n: sorted.len(),
    }
}

/// A directory under the target directory, emptied first.
///
/// Under `target` rather than in a temporary directory because a probe whose numbers depend on which
/// filesystem it wrote to should write where the rest of the build writes, which is the disk
/// somebody would point a real cache at.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("cold-start")
        .join(name);
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("the target directory is writable");
    dir
}

/// How big the two forms of the decoder are.
///
/// Here because somebody deciding how much disk to give a cache directory needs it, and because the
/// ratio between the two is the answer to why the pool weighs modules and not artefacts. It is the
/// same multiple for every decoder an engine compiles, so weighing the small one orders entries the
/// way weighing the large one would.
struct Sizes {
    module: u64,
    artefact: u64,
}

/// Fills a directory once and reads back what went into it.
fn sizes(bytes: &[u8]) -> Sizes {
    let dir = scratch("sizes");
    Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(&dir)
        .open(bytes)
        .expect("the container opens");

    let artefact = fs::read_dir(&dir)
        .expect("the directory is readable")
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".iris-aot"))
        .and_then(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .expect("the open left one artefact");

    drop(fs::remove_dir_all(&dir));
    Sizes {
        module: u64::try_from(support::decoder_module().len()).expect("a module fits"),
        artefact,
    }
}

/// One measured shape.
struct Measured {
    name: &'static str,
    why: &'static str,
    open: Summary,
}

/// Opens the container with a fresh runtime every time and no cache anywhere.
fn no_cache(bytes: &[u8], repeats: usize) -> Summary {
    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let runtime = Runtime::new().expect("a runtime starts");
        let started = Instant::now();
        black_box(runtime.open(black_box(bytes)).expect("the container opens"));
        samples.push(started.elapsed().as_secs_f64() * 1e3);
    }
    summarise(&samples)
}

/// Opens the container with a fresh runtime and a fresh empty directory every time.
fn cold(bytes: &[u8], repeats: usize) -> Summary {
    let mut samples = Vec::with_capacity(repeats);
    for repeat in 0..repeats {
        let dir = scratch(&format!("cold-{repeat}"));
        let runtime = Runtime::new()
            .expect("a runtime starts")
            .with_compilation_cache(&dir);
        let started = Instant::now();
        black_box(runtime.open(black_box(bytes)).expect("the container opens"));
        samples.push(started.elapsed().as_secs_f64() * 1e3);
        assert_eq!(runtime.compilations_stored(), 1, "a cold open has to store");
        drop(fs::remove_dir_all(&dir));
    }
    summarise(&samples)
}

/// Opens the container with a fresh runtime against a directory that already has the artefact.
fn warm(bytes: &[u8], repeats: usize) -> Summary {
    let dir = scratch("warm");
    Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(&dir)
        .open(bytes)
        .expect("the container opens");

    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let runtime = Runtime::new()
            .expect("a runtime starts")
            .with_compilation_cache(&dir);
        let started = Instant::now();
        black_box(runtime.open(black_box(bytes)).expect("the container opens"));
        samples.push(started.elapsed().as_secs_f64() * 1e3);
        assert_eq!(
            runtime.compilations_reused(),
            1,
            "a warm open has to be a hit, or this shape is measuring the cold one"
        );
    }
    summarise(&samples)
}

/// Opens the container repeatedly with one runtime, so the pool answers every time after the first.
fn pooled(bytes: &[u8], repeats: usize) -> Summary {
    let runtime = Runtime::new().expect("a runtime starts");
    runtime.open(bytes).expect("the container opens");

    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let started = Instant::now();
        black_box(runtime.open(black_box(bytes)).expect("the container opens"));
        samples.push(started.elapsed().as_secs_f64() * 1e3);
    }
    assert_eq!(
        runtime.decoders_compiled(),
        1,
        "the pool has to be answering, or this shape is measuring something else"
    );
    summarise(&samples)
}

fn human(target: &str, rows: u64, sizes: &Sizes, measured: &[Measured]) {
    println!("Opening a container on {target}, {rows} rows and {COLUMNS} columns");
    println!();
    println!("Every shape opens the container and stops. Nothing here scans, because a scan would");
    println!("hide a fixed cost per open under the rows it read.");
    println!();
    println!(
        "{:<10} {:>11} {:>22}  why it is here",
        "shape", "median ms", "spread ms"
    );
    for m in measured {
        println!(
            "{:<10} {:>11.2} {:>10.2} to {:>8.2}  {}",
            m.name, m.open.median, m.open.lo, m.open.hi, m.why
        );
    }
    println!();

    let plain = measured
        .iter()
        .find(|m| m.name == "no cache")
        .expect("the probe measured it");
    let hot = measured
        .iter()
        .find(|m| m.name == "warm")
        .expect("the probe measured it");
    let floor = measured
        .iter()
        .find(|m| m.name == "pooled")
        .expect("the probe measured it");

    let saved = plain.open.median - hot.open.median;
    println!(
        "A warm open saves {:.2} ms of {:.2}, which is {:.1} percent.",
        saved,
        plain.open.median,
        saved / plain.open.median * 100.0
    );
    println!(
        "What is left over the pooled floor is {:.2} ms, so reading the artefact back is that much",
        hot.open.median - floor.open.median
    );
    println!("more than having the module already compiled in the process.");
    println!();
    println!(
        "The decoder module is {} bytes and its artefact is {} bytes, which is {:.1} times as big.",
        sizes.module,
        sizes.artefact,
        f64::from(u32::try_from(sizes.artefact).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(sizes.module).unwrap_or(1).max(1))
    );
}

fn summary_json(label: &str, s: &Summary) -> String {
    format!(
        "\"{label}\":{{\"median_ms\":{:.4},\"lo_ms\":{:.4},\"hi_ms\":{:.4},\"n\":{}}}",
        s.median, s.lo, s.hi, s.n
    )
}

fn json(target: &str, rows: u64, sizes: &Sizes, measured: &[Measured]) {
    let shapes: Vec<String> = measured
        .iter()
        .map(|m| {
            format!(
                "{{\"shape\":\"{}\",{}}}",
                m.name,
                summary_json("open", &m.open)
            )
        })
        .collect();
    println!(
        "{{\"probe\":\"cold_start\",\"target\":\"{target}\",\"rows\":{rows},\
         \"columns\":{COLUMNS},\"module_bytes\":{},\"artefact_bytes\":{},\"shapes\":[{}]}}",
        sizes.module,
        sizes.artefact,
        shapes.join(",")
    );
}

fn parse_arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|at| args.get(at + 1))
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let rows = parse_arg(&args, "--rows", DEFAULT_ROWS).max(1);
    let repeats = parse_arg(&args, "--repeats", DEFAULT_REPEATS).max(3);
    let target = format!("{}-{}", env::consts::OS, env::consts::ARCH);

    let bytes = builder(rows, COLUMNS)
        .build()
        .expect("a container this size fits");

    let sizes = sizes(&bytes);
    let measured = vec![
        Measured {
            name: "no cache",
            why: "a fresh process with nowhere to keep a compiled decoder, which is every start \
                  before this existed",
            open: no_cache(&bytes, repeats),
        },
        Measured {
            name: "cold",
            why: "a fresh process against an empty directory, which is the first start after a \
                  deployment and pays for the write",
            open: cold(&bytes, repeats),
        },
        Measured {
            name: "warm",
            why: "a fresh process against a directory that already has the artefact, which is \
                  every start after the first",
            open: warm(&bytes, repeats),
        },
        Measured {
            name: "pooled",
            why: "the same process opening it again, so nothing is compiled or looked up and this \
                  is the floor",
            open: pooled(&bytes, repeats),
        },
    ];

    if args.iter().any(|a| a == "--json") {
        json(&target, rows, &sizes, &measured);
    } else {
        human(&target, rows, &sizes, &measured);
    }

    drop(fs::remove_dir_all(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("cold-start"),
    ));
}
