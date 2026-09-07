//! Is the WebAssembly sandbox cheaper on arm64 than on x86-64?
//!
//! The M5 gate calls this the most interesting unrun experiment in this area, and the reason is
//! arithmetic. WebAssembly's vector width is capped at 128 bits and is going to stay there for the
//! foreseeable future. Arm Neon is 128 bits. AVX2 is 256, and AVX-512 is 512. So on x86-64 a native
//! decoder can have vectors two or four times the width the guest can ever have, and on arm64 it
//! has exactly the same width. If the gap between running a decoder in the sandbox and running it
//! natively is mostly a vector width gap, then it should be much smaller on arm64, and the whole
//! shape of the argument for the native fast path changes with it.
//!
//! The prior art ran its entire evaluation on one Intel machine, so as far as we can tell nobody
//! has checked.
//!
//! # What is compared
//!
//! One decode kernel, `m5_decode::decode_part`, which parses a `BtrBlocks` column part, decodes
//! every chunk in it and folds a sample of the values into a checksum. It is built twice from the
//! same source: to wasm32 as a `cdylib` that runs under Wasmtime, and into this binary as an `rlib`
//! that is called directly. Both sides return the checksum and the probe refuses to report a case
//! where they disagree, so a ratio here is two builds of one decoder rather than two decoders.
//!
//! The sampling is deliberate and that crate's documentation says why at length. The short version
//! is that folding every value made the fold the loop, and a fold is a serial dependency chain that
//! vectorises nowhere, so leaving it in would have answered a vector width question with a fact
//! about the checksum.
//!
//! Three sides, not two.
//!
//! `guest-simd128` is the module compiled with the WebAssembly SIMD proposal available, which is
//! what a decoder shipped today would be built with. `guest-scalar` is the same source with that
//! turned off. The difference between them is what WebAssembly SIMD is worth on this kernel, and it
//! is the control that stops the whole experiment resting on an assumption: if the two are the same
//! then nothing here vectorises and no vector width argument applies, whatever the architecture.
//! `native` is the host build, and what it was allowed to use depends on how this binary was
//! compiled, which is why the report prints that rather than assuming it.
//!
//! # Reading the ratio
//!
//! Run this twice on the same machine. Once built for the machine it is on, with
//! `RUSTFLAGS=-C target-cpu=native`, and once at the baseline for the architecture. On x86-64 those
//! are two different vector widths and on arm64 they are both 128 bits, so the pair of ratios is the
//! measurement and either one on its own is not.
//!
//! The corpus is `conformance/btrblocks/fixtures`, the same parts the `iris-btr` conformance suite
//! reads, which are produced by linking against the reference implementation. Every scheme the
//! reference picks by default is in there, so the report is per scheme rather than one number: bit
//! packing is the part expected to vectorise and a dictionary lookup is the part expected not to,
//! and averaging those together would hide the mechanism this experiment exists to identify.
//!
//! # What this probe does not measure
//!
//! Instructions retired and instructions per cycle, which the gate also asks for. Those need a
//! hardware counter, this process cannot read one portably, and a number invented from a duration
//! would be worse than none. `ci/vector_counters.py` runs this under `perf stat` at two repeat
//! counts and subtracts, so that everything that is not the measured loop cancels. That script is
//! where the counter half of the answer comes from, and it needs `--only` and `--repeats`, which is
//! what those two options are for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// How many times each side decodes each part before anything is timed.
///
/// The guest side needs it more than the host side does. The first call into a freshly instantiated
/// module touches code Cranelift has compiled but the machine has never executed, and on a part this
/// small that page in is a visible share of one iteration.
const WARMUP: usize = 20;

/// How many timed iterations each side runs, unless `--repeats` says otherwise.
const DEFAULT_REPEATS: usize = 200;

/// The corpus, relative to the repository root.
const CORPUS: &str = "conformance/btrblocks/fixtures";

// ---------------------------------------------------------------------------------------------
// The sides
// ---------------------------------------------------------------------------------------------

/// One of the three things being timed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The module built with the WebAssembly SIMD proposal available.
    GuestSimd,
    /// The same module built without it.
    GuestScalar,
    /// This binary, calling the kernel directly.
    Native,
}

impl Side {
    /// The name the report and the JSON use.
    const fn name(self) -> &'static str {
        match self {
            Self::GuestSimd => "guest-simd128",
            Self::GuestScalar => "guest-scalar",
            Self::Native => "native",
        }
    }

    /// Every side, in the order they are reported.
    const ALL: [Self; 3] = [Self::GuestSimd, Self::GuestScalar, Self::Native];

    /// The one `--only` names, if it named one this probe knows.
    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|side| side.name() == text)
    }
}

// ---------------------------------------------------------------------------------------------
// The guest
// ---------------------------------------------------------------------------------------------

/// A compiled module with its store, ready to be handed bytes and asked for a checksum.
struct Guest {
    store: Store<()>,
    memory: Memory,
    reserve: TypedFunc<u32, u32>,
    decode: TypedFunc<(), u64>,
}

impl std::fmt::Debug for Guest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guest").finish_non_exhaustive()
    }
}

impl Guest {
    /// Instantiates the module. It imports nothing, which is the point of the two export interface.
    fn new(engine: &Engine, wasm: &[u8]) -> wasmtime::Result<Self> {
        let module = Module::new(engine, wasm)?;
        let mut store = Store::new(engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("the guest exports no memory called memory"))?;
        let reserve = instance.get_typed_func::<u32, u32>(&mut store, "reserve")?;
        let decode = instance.get_typed_func::<(), u64>(&mut store, "decode")?;
        Ok(Self {
            store,
            memory,
            reserve,
            decode,
        })
    }

    /// Writes a part into linear memory once, so the timed loop does not pay for the copy.
    ///
    /// The kernel does not modify the bytes it was handed, so one write is enough for any number of
    /// decodes of the same part. Copying on every iteration would put a `memcpy` on the guest side
    /// of the ratio that the host side does not have.
    fn load(&mut self, part: &[u8]) -> wasmtime::Result<()> {
        let len = u32::try_from(part.len())
            .map_err(|_| wasmtime::Error::msg("a corpus part is larger than a wasm32 address"))?;
        let at = self.reserve.call(&mut self.store, len)?;
        self.memory.write(&mut self.store, at as usize, part)?;
        Ok(())
    }

    /// Decodes what was loaded and returns the checksum.
    fn run(&mut self) -> wasmtime::Result<u64> {
        self.decode.call(&mut self.store, ())
    }
}

/// Compiles `m5-decode` for wasm32 and hands back the module bytes.
///
/// Built here rather than checked in, for the reason the M0 probe gives at more length: a committed
/// `.wasm` is a binary nobody reads, produced by a toolchain nobody remembers, that keeps being
/// measured after the source it came from has stopped matching it.
///
/// The nested cargo gets its own target directory per feature set, because cargo's lock is per
/// target directory and building into the one the outer cargo holds would deadlock rather than fail,
/// and because building the same crate twice with different flags into one directory is a rebuild
/// each time rather than two artifacts.
fn build_guest(simd: bool) -> wasmtime::Result<Vec<u8>> {
    let root = repository_root()?;
    let suffix = if simd { "simd" } else { "scalar" };
    let target_dir = root.join("target").join(format!("m5-decode-{suffix}"));

    let mut cargo = Command::new(env!("CARGO"));
    cargo
        .current_dir(&root)
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "m5-decode",
            "--target",
            "wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target_dir);

    // Whatever this binary was built with is about to be the variable in the experiment, and none of
    // it applies to wasm32 anyway, so it is cleared rather than inherited. The one flag that is set
    // here is the one the two guest builds differ by.
    for leaked in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "LLVM_PROFILE_FILE",
    ] {
        cargo.env_remove(leaked);
    }
    if simd {
        cargo.env("RUSTFLAGS", "-C target-feature=+simd128");
    }
    // The compiler is named rather than left to be found. `env!("CARGO")` is the cargo that built
    // this binary, and a cargo run that way looks for `rustc` on PATH, which on a machine whose
    // default toolchain is not the pinned one is a different toolchain that has never been asked to
    // install the wasm32 target. The failure is `can't find crate for core`, which reads as a missing
    // target and is not one. The rustc next to the cargo that built this is by definition the one
    // this was built with.
    if let Some(rustc) = sibling_rustc() {
        cargo.env("RUSTC", rustc);
    }

    let out = cargo.output()?;
    if !out.status.success() {
        wasmtime::bail!(
            "building m5-decode for wasm32 failed. If the target is missing, run\n  \
             rustup target add wasm32-unknown-unknown\n\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let path = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("m5_decode.wasm");
    Ok(std::fs::read(&path)?)
}

/// The `rustc` that sits next to the `cargo` that built this binary, if there is one.
///
/// Under rustup both of these are either proxies, in which case they resolve to the same toolchain
/// by the same rules, or they are the real binaries inside one toolchain directory. Either way the
/// pair is consistent, which is the whole point of picking it this way.
fn sibling_rustc() -> Option<PathBuf> {
    let rustc =
        Path::new(env!("CARGO"))
            .parent()?
            .join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
    rustc.is_file().then_some(rustc)
}

/// Where the repository is, from where this example's manifest is.
fn repository_root() -> std::io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
}

// ---------------------------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------------------------

/// One part from the corpus, with the name it is reported under.
#[derive(Clone, Debug)]
struct Case {
    /// The fixture's stem, which names the column type and the scheme.
    name: String,
    /// The compressed part.
    bytes: Vec<u8>,
}

/// Reads every `.btr` fixture, sorted, optionally filtered by a substring.
///
/// An exact name wins over the substring, so that asking for `dbl-pseudodecimal` gets that case
/// and not that case plus `dbl-pseudodecimal-some-null`. The counter script divides by the number
/// of cases it ran, so a filter that quietly matches two of them reports the average of a pair
/// under the name of one, which is a wrong number rather than a missing one.
fn corpus(filter: Option<&str>) -> std::io::Result<Vec<Case>> {
    let dir = repository_root()?.join(CORPUS);
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("btr") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if filter.is_some_and(|want| !name.contains(want)) {
            continue;
        }
        cases.push(Case {
            name: name.to_owned(),
            bytes: std::fs::read(&path)?,
        });
    }
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(want) = filter
        && cases.iter().any(|case| case.name == want)
    {
        cases.retain(|case| case.name == want);
    }
    Ok(cases)
}

// ---------------------------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------------------------

/// What one side did on one case.
#[derive(Clone, Debug)]
struct Timing {
    /// Median of the timed iterations, in microseconds.
    median_us: f64,
    /// Fastest and slowest of them, so the median is read with its spread.
    lo_us: f64,
    hi_us: f64,
    /// What the kernel returned, which has to match across sides.
    checksum: u64,
}

/// The middle of a sorted set, or the lower of the two middles when there are an even number.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(f64::NAN)
}

/// Warms up, then times `repeats` iterations of one closure.
fn time(repeats: usize, mut body: impl FnMut() -> u64) -> Timing {
    let mut checksum = 0;
    for _ in 0..WARMUP {
        checksum = body();
    }
    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let started = Instant::now();
        checksum = std::hint::black_box(body());
        samples.push(started.elapsed().as_secs_f64() * 1e6);
    }
    let lo = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Timing {
        median_us: median(&mut samples),
        lo_us: lo,
        hi_us: hi,
        checksum,
    }
}

/// Everything measured for one case.
#[derive(Clone, Debug)]
struct Row {
    /// The fixture's stem.
    name: String,
    /// The compressed size, which is what each side was handed.
    bytes: usize,
    /// One timing per side that ran, keyed by the side's name.
    sides: BTreeMap<&'static str, Timing>,
}

impl Row {
    /// The ratio of one side to another, or `None` if either was not run.
    fn ratio(&self, over: Side, under: Side) -> Option<f64> {
        let a = self.sides.get(over.name())?;
        let b = self.sides.get(under.name())?;
        Some(a.median_us / b.median_us)
    }
}

// ---------------------------------------------------------------------------------------------
// What the host was built with
// ---------------------------------------------------------------------------------------------

/// The widest vector unit this binary was compiled to use, as a name and a width in bits.
///
/// Read from `cfg!(target_feature = ...)`, which is the compiler answering about this build rather
/// than the machine answering about itself. That is the right question: what matters to the ratio is
/// what the native side was allowed to emit, and a machine with AVX-512 running a binary built for
/// the baseline is a 128 bit native side no matter what the machine can do.
fn host_vectors() -> (&'static str, u32) {
    if cfg!(target_feature = "avx512f") {
        ("avx512f", 512)
    } else if cfg!(target_feature = "avx2") {
        ("avx2", 256)
    } else if cfg!(target_feature = "avx") {
        ("avx", 256)
    } else if cfg!(target_feature = "sse2") {
        ("sse2", 128)
    } else if cfg!(target_feature = "neon") {
        ("neon", 128)
    } else {
        ("none declared", 0)
    }
}

/// The architecture and operating system, for the report and for telling two runs apart.
fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

// ---------------------------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------------------------

/// What the command line asked for.
#[derive(Debug)]
struct Options {
    repeats: usize,
    only: Option<Side>,
    filter: Option<String>,
    json: bool,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            repeats: DEFAULT_REPEATS,
            only: None,
            filter: None,
            json: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => options.json = true,
                "--repeats" => {
                    let value = args.next().ok_or("--repeats wants a number")?;
                    options.repeats = value.parse().map_err(|_| "--repeats wants a number")?;
                }
                "--only" => {
                    let value = args.next().ok_or("--only wants a side")?;
                    options.only = Some(Side::parse(&value).ok_or_else(|| {
                        format!(
                            "--only wants one of {}, not {value}",
                            Side::ALL.map(Side::name).join(", ")
                        )
                    })?);
                }
                "--cases" => {
                    options.filter = Some(args.next().ok_or("--cases wants a substring")?);
                }
                other => return Err(format!("{other} is not an option this probe takes")),
            }
        }
        if options.repeats == 0 {
            return Err("--repeats wants at least one".to_owned());
        }
        Ok(options)
    }

    /// Whether a side runs, which `--only` narrows.
    fn runs(&self, side: Side) -> bool {
        self.only.is_none_or(|only| only == side)
    }
}

fn main() -> wasmtime::Result<()> {
    let options = match Options::parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // The guest is always built with `--release`, because nobody ships an unoptimised decoder and
    // there would be nothing to learn from measuring one. So a debug build of this probe puts an
    // optimised guest against an unoptimised host and reports the sandbox as several times faster
    // than native, which is a fact about the profile and about nothing else. Refuse rather than
    // print it.
    if cfg!(debug_assertions) {
        wasmtime::bail!(
            "this probe has to be built with --release, because the guest side always is and a \
             ratio between two profiles measures the profiles"
        );
    }

    let cases = corpus(options.filter.as_deref())?;
    if cases.is_empty() {
        wasmtime::bail!("no fixtures matched, so there is nothing to measure");
    }

    let engine = Engine::default();
    let mut guests: BTreeMap<&'static str, Guest> = BTreeMap::new();
    for (side, simd) in [(Side::GuestSimd, true), (Side::GuestScalar, false)] {
        if options.runs(side) {
            guests.insert(side.name(), Guest::new(&engine, &build_guest(simd)?)?);
        }
    }

    let rows = measure(&options, &cases, &mut guests)?;
    if options.json {
        json(&options, &rows);
    } else {
        human(&options, &rows);
    }
    Ok(())
}

/// Times every side that is running against every case.
fn measure(
    options: &Options,
    cases: &[Case],
    guests: &mut BTreeMap<&'static str, Guest>,
) -> wasmtime::Result<Vec<Row>> {
    let mut rows = Vec::with_capacity(cases.len());
    for case in cases {
        let mut sides = BTreeMap::new();

        for (name, guest) in guests.iter_mut() {
            guest.load(&case.bytes)?;
            // The `?` cannot live inside the timed closure, so a failure is turned into a checksum
            // of zero and caught by the agreement check below rather than swallowed.
            sides.insert(*name, time(options.repeats, || guest.run().unwrap_or(0)));
        }

        if options.runs(Side::Native) {
            sides.insert(
                Side::Native.name(),
                time(options.repeats, || {
                    m5_decode::decode_part(&case.bytes).unwrap_or(0)
                }),
            );
        }

        // Every side has to have produced the same answer or the ratio is between two different
        // pieces of work. This is checked per case and it is fatal, because a probe that reports a
        // number next to a disagreement is a probe whose number nobody can use.
        let mut checksums = sides.values().map(|t| t.checksum);
        let first = checksums.next().unwrap_or_default();
        if first == 0 || !checksums.all(|c| c == first) {
            wasmtime::bail!(
                "the sides disagree on {}: {:?}",
                case.name,
                sides
                    .iter()
                    .map(|(name, t)| (*name, t.checksum))
                    .collect::<Vec<_>>()
            );
        }

        rows.push(Row {
            name: case.name.clone(),
            bytes: case.bytes.len(),
            sides,
        });
    }
    Ok(rows)
}

/// The geometric mean of a set of ratios.
///
/// Geometric rather than arithmetic, because these are ratios. The arithmetic mean of a two times
/// slowdown and a half times speedup is 1.25, which says the sandbox costs something when the two
/// cases cancelled exactly. The geometric mean says one, which is the true answer.
fn geomean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let sum: f64 = values.iter().map(|v| v.ln()).sum();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count of corpus cases, which is under a hundred"
    )]
    let n = values.len() as f64;
    (sum / n).exp()
}

fn human(options: &Options, rows: &[Row]) {
    let (vectors, width) = host_vectors();
    println!("The WebAssembly sandbox against the host, one decoder, per scheme");
    println!();
    println!("Target: {}.", target());
    println!(
        "The native side was built for {vectors}, which is {width} bit vectors. The guest is capped \
         at 128 whatever the machine is."
    );
    println!(
        "Repeats: {}, after {WARMUP} warmup passes, median reported with the fastest and slowest \
         next to it.",
        options.repeats
    );
    println!("Corpus: {CORPUS}, one part per case, the same parts the conformance suite reads.");
    println!();

    let sides: Vec<&'static str> = Side::ALL
        .into_iter()
        .filter(|s| options.runs(*s))
        .map(Side::name)
        .collect();

    print!("{:<28} {:>9}", "case", "bytes");
    for side in &sides {
        print!(" {side:>15}");
    }
    if options.only.is_none() {
        print!(" {:>10} {:>10}", "simd/native", "simd/scalar");
    }
    println!();

    for row in rows {
        print!("{:<28} {:>9}", row.name, row.bytes);
        for side in &sides {
            match row.sides.get(side) {
                Some(t) => print!(" {:>15.1}", t.median_us),
                None => print!(" {:>15}", "-"),
            }
        }
        if options.only.is_none() {
            let over = row.ratio(Side::GuestSimd, Side::Native).unwrap_or(f64::NAN);
            let simd = row
                .ratio(Side::GuestSimd, Side::GuestScalar)
                .unwrap_or(f64::NAN);
            print!(" {over:>10.2} {simd:>10.2}");
        }
        println!();
    }
    println!();
    println!(
        "Times are microseconds per decode of one part. Ratios are the column over the column."
    );
    println!();

    if options.only.is_some() {
        println!(
            "One side only, so there is no ratio here. This shape is for running under a counter, \
             where the point is that nothing else is in the process."
        );
        return;
    }

    let over: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.ratio(Side::GuestSimd, Side::Native))
        .collect();
    let simd: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.ratio(Side::GuestSimd, Side::GuestScalar))
        .collect();

    println!(
        "Across {} cases the guest is {:.2} times the host, geometric mean, worst case {:.2} and \
         best case {:.2}.",
        over.len(),
        geomean(&over),
        over.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        over.iter().copied().fold(f64::INFINITY, f64::min),
    );
    println!(
        "WebAssembly SIMD is worth {:.2} times on this kernel, geometric mean of the SIMD build \
         over the scalar one. A number near one means nothing here vectorises in the guest, and \
         then no vector width argument applies whatever the machine has.",
        geomean(&simd),
    );
    println!();
    println!(
        "This is one half of the measurement. Run it again on the same machine with the other \
         target-cpu, because the pair of ratios is what says whether the gap is vector width, and \
         either one on its own does not."
    );
}

fn json(options: &Options, rows: &[Row]) {
    let (vectors, width) = host_vectors();
    let cases: Vec<String> = rows
        .iter()
        .map(|row| {
            let sides: Vec<String> = row
                .sides
                .iter()
                .map(|(name, t)| {
                    format!(
                        "\"{name}\":{{\"median_us\":{:.3},\"lo_us\":{:.3},\"hi_us\":{:.3},\
                         \"checksum\":{}}}",
                        t.median_us, t.lo_us, t.hi_us, t.checksum
                    )
                })
                .collect();
            format!(
                "{{\"case\":\"{}\",\"bytes\":{},\"sides\":{{{}}}}}",
                row.name,
                row.bytes,
                sides.join(",")
            )
        })
        .collect();

    let over: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.ratio(Side::GuestSimd, Side::Native))
        .collect();
    let simd: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.ratio(Side::GuestSimd, Side::GuestScalar))
        .collect();

    println!(
        "{{\"probe\":\"m5_vector\",\"target\":\"{}\",\"host_vectors\":\"{vectors}\",\
         \"host_vector_bits\":{width},\"guest_vector_bits\":128,\"repeats\":{},\"warmup\":{WARMUP},\
         \"only\":{},\"guest_over_native\":{:.4},\"simd_over_scalar\":{:.4},\"cases\":[{}]}}",
        target(),
        options.repeats,
        options
            .only
            .map_or_else(|| "null".to_owned(), |s| format!("\"{}\"", s.name())),
        geomean(&over),
        geomean(&simd),
        cases.join(",")
    );
}
