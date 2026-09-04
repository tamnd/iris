//! What `iris-guard` costs, measured against the work it protects.
//!
//! The decision rule for this number was written down before the number existed, in the issue that
//! owns this probe, so that the answer could not be negotiated afterwards. Under five percent, the
//! guard stays on and nothing more is said. Between five and fifteen, it stays on and digest pinning
//! gets documented as the normal path for a host that needs the difference. Over fifteen, it stays
//! on anyway and the cost becomes a design problem rather than a check to remove. There is no branch
//! in which the guard comes off, which is what makes it a gate and not a benchmark.
//!
//! Run it with `cargo run --release -p iris-runtime --features probe --example guard_cost`. Add
//! `--json` for a machine readable object suitable for handing to iris-bench.
//!
//! # What is being divided by what
//!
//! The denominator is assembly: taking a checked batch and building Arrow arrays out of it. That is
//! the tightest denominator available and it is deliberately the one that shows the guard in its
//! worst light. The number a host would actually feel is the guard against a whole scan, which
//! includes decoding inside the sandbox, and decoding is where the time in a real workload goes. So
//! a share reported here is an upper bound on the share of a scan, by some margin.
//!
//! Assembly is not free of checking either, because `ArrayData::try_new` validates what it is
//! handed. That second pass stays in the denominator rather than being subtracted out. Removing it
//! would mean building arrays unchecked, which is an `unsafe` call this crate forbids, so it is part
//! of the cost of getting a batch into Arrow whatever the guard does.
//!
//! # Plain and encoded
//!
//! This comparison went in expecting the encoded case to be the cheap one, on the reasoning that a
//! dictionary is less data and less data is less to walk. The measurement says the opposite, by two
//! orders of magnitude, and it is worth writing down why rather than quietly reporting the number.
//!
//! Checking a plain column of fixed width values is one multiplication. The guard asks whether the
//! buffer is at least a length times a width, and nothing about the values themselves can be out of
//! bounds, because every bit pattern of eight bytes is a valid `i64`. Checking a dictionary is a
//! comparison per key, because a key is a position into something else and a position can be wrong.
//! So the encoded form is smaller and costs more to check, and the thing that decides the cost is
//! not how much data there is but whether the data is an index.
//!
//! That is the useful shape of it for anybody choosing an encoding: the guard charges for
//! indirection, not for bytes. It is also why the string case above is the expensive one, since
//! offsets are indices too.
//!
//! The encoded family is measured at the guard rather than end to end, because this runtime refuses
//! dictionary columns today. That refusal is a container format question rather than a guard
//! question, and the guard's dictionary check is written, tested and fuzzed already.

use std::env;
use std::hint::black_box;
use std::time::Instant;

use arrow_schema::{DataType, Field, Schema};
use iris_abi::Node;
use iris_runtime::probe::{build, record_batch};
use iris_vm::RawBatch;

/// How many rows a measured batch holds.
///
/// Eight thousand is the runtime's default batch size, so this is the size the guard is actually
/// asked about rather than a size chosen to make a ratio look a particular way.
const ROWS: u64 = 8192;

/// How many columns a measured batch holds.
const COLUMNS: usize = 3;

/// How many distinct values the dictionary case has.
const DICTIONARY: u64 = 256;

/// The share of assembly below which the guard costs nothing worth discussing.
const SETTLED_GATE: f64 = 0.05;

/// The share above which the cost stops being a footnote and becomes a design problem.
const DESIGN_GATE: f64 = 0.15;

/// A summarised sample set, in nanoseconds.
struct Summary {
    /// Median of the samples.
    median: f64,
    /// Lower bound of the 95 percent bootstrap interval on the median.
    lo: f64,
    /// Upper bound of the 95 percent bootstrap interval on the median.
    hi: f64,
    /// How many samples produced it.
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

/// Percentile bootstrap on the median.
///
/// The same shape as the M0 probe uses, down to the fixed seed, so that two probes in this
/// repository do not report intervals that mean subtly different things. Ten thousand resamples is
/// more than the interval needs and cheap enough not to bother tuning.
fn summarise(samples: &[f64]) -> Summary {
    const RESAMPLES: usize = 10_000;
    const LO_INDEX: usize = RESAMPLES / 40;
    const HI_INDEX: usize = RESAMPLES - RESAMPLES / 40 - 1;

    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let point = median_of(&sorted);

    let n = u64::try_from(sorted.len()).unwrap_or(1).max(1);
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut medians = Vec::with_capacity(RESAMPLES);
    let mut draw = vec![0.0f64; sorted.len()];
    for _ in 0..RESAMPLES {
        for slot in &mut draw {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = sorted[usize::try_from(state % n).unwrap_or(0)];
        }
        draw.sort_by(f64::total_cmp);
        medians.push(median_of(&draw));
    }
    medians.sort_by(f64::total_cmp);

    Summary {
        median: point,
        lo: medians[LO_INDEX],
        hi: medians[HI_INDEX],
        n: sorted.len(),
    }
}

/// Times one closure the given number of times, throwing away a warmup pass each time.
fn samples(count: usize, mut body: impl FnMut()) -> Vec<f64> {
    // A handful of untimed passes first, so the first sample is not paying for a cold cache and a
    // branch predictor that has never seen this code.
    for _ in 0..8 {
        body();
    }

    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        body();
        out.push(started.elapsed().as_secs_f64() * 1e9);
    }
    out
}

/// One shape of batch, and what it is here to say.
struct Shape {
    /// What the shape is called in the output.
    name: &'static str,
    /// Why it is in the set, which is always a different thing for the guard to walk.
    why: &'static str,
    /// The schema the batch is checked against.
    schema: Schema,
    /// The batch.
    batch: RawBatch,
}

/// Three columns of non-nullable `i64`, which is the cheapest thing a decoder can produce.
///
/// The guard walks three nodes and six buffers and multiplies a length by a width, and assembly
/// copies a hundred and ninety two kilobytes into aligned Arrow buffers. This looked like the shape
/// that would decide the headline, on the grounds that a denominator with almost nothing in it is
/// where a fixed cost shows up worst. It is the cheapest row in the table instead, because the
/// guard's work here is constant and assembly's is not.
fn plain() -> Shape {
    let schema = Schema::new(
        (0..COLUMNS)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, false))
            .collect::<Vec<_>>(),
    );

    let values: Vec<u8> = (0..ROWS)
        .flat_map(|v| i64::try_from(v).expect("8192 fits").to_le_bytes())
        .collect();
    let mut buffers = Vec::with_capacity(COLUMNS * 2);
    for _ in 0..COLUMNS {
        buffers.push(Vec::new());
        buffers.push(values.clone());
    }

    Shape {
        name: "int64-plain",
        why: "three non-nullable integer columns, the cheapest batch a decoder can produce",
        schema,
        batch: RawBatch {
            rows: ROWS,
            nodes: vec![
                Node {
                    length: ROWS,
                    null_count: 0
                };
                COLUMNS
            ],
            buffers,
        },
    }
}

/// The same, with a validity bitmap on every column.
///
/// Nulls give the guard another buffer per array to bound and give assembly another buffer per
/// array to copy, so this is here to show whether the ratio moves when both sides get more work
/// rather than one.
fn nullable() -> Shape {
    let schema = Schema::new(
        (0..COLUMNS)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, true))
            .collect::<Vec<_>>(),
    );

    let values: Vec<u8> = (0..ROWS)
        .flat_map(|v| i64::try_from(v).expect("8192 fits").to_le_bytes())
        .collect();
    let validity = vec![0xffu8; usize::try_from(ROWS).expect("8192 fits") / 8];
    let mut buffers = Vec::with_capacity(COLUMNS * 2);
    for _ in 0..COLUMNS {
        buffers.push(validity.clone());
        buffers.push(values.clone());
    }

    Shape {
        name: "int64-nullable",
        why: "the same columns with a validity bitmap, so both halves get one more buffer each",
        schema,
        batch: RawBatch {
            rows: ROWS,
            nodes: vec![
                Node {
                    length: ROWS,
                    null_count: 0
                };
                COLUMNS
            ],
            buffers,
        },
    }
}

/// One `Utf8` column, which is the shape where the guard has real work to do.
///
/// Every offset has to be inside the values buffer and the offsets have to be non-decreasing, which
/// is a pass over the offset buffer rather than one multiplication. This is the guard's expensive
/// case and it is the one worth reporting next to the cheap one.
fn strings() -> Shape {
    let schema = Schema::new(vec![Field::new("s", DataType::Utf8, false)]);

    let word = b"benchmark";
    let mut offsets = Vec::with_capacity((usize::try_from(ROWS).expect("8192 fits") + 1) * 4);
    let mut values = Vec::with_capacity(usize::try_from(ROWS).expect("8192 fits") * word.len());
    for row in 0..ROWS {
        let at = i32::try_from(row).expect("8192 fits") * i32::try_from(word.len()).expect("nine");
        offsets.extend_from_slice(&at.to_le_bytes());
        values.extend_from_slice(word);
    }
    offsets.extend_from_slice(
        &(i32::try_from(values.len()).expect("a batch this size fits")).to_le_bytes(),
    );

    Shape {
        name: "utf8",
        why: "one string column, so the guard walks every offset rather than one length",
        schema,
        batch: RawBatch {
            rows: ROWS,
            nodes: vec![Node {
                length: ROWS,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), offsets, values],
        },
    }
}

/// What one shape cost.
struct Measured {
    name: &'static str,
    why: &'static str,
    guard: Summary,
    build: Summary,
}

impl Measured {
    /// The guard as a share of getting this batch into Arrow.
    fn share(&self) -> f64 {
        self.guard.median / (self.guard.median + self.build.median)
    }
}

fn measure(shape: &Shape, count: usize) -> Measured {
    let Shape {
        name,
        why,
        schema,
        batch,
    } = shape;

    // Both halves are run once here rather than trusted, because a probe that measures a path which
    // is failing is measuring an error return. A shape that does not assemble is a bug in the probe.
    record_batch(&schema.clone().into(), batch).expect("a shape in this probe assembles");

    let schema_ref = schema.clone().into();
    let guard = summarise(&samples(count, || {
        black_box(iris_guard::check(
            black_box(schema),
            batch.rows,
            &batch.nodes,
            &batch.buffers,
        ))
        .expect("a shape in this probe passes the guard");
    }));

    let build = summarise(&samples(count, || {
        black_box(build(&schema_ref, black_box(batch))).expect("a shape in this probe assembles");
    }));

    Measured {
        name,
        why,
        guard,
        build,
    }
}

/// What checking a dictionary costs against checking the plain array it stands for.
struct Encoded {
    keys: Summary,
    plain: Summary,
}

impl Encoded {
    /// What the encoded case costs against the plain one, as a ratio. Below one is cheaper.
    fn ratio(&self) -> f64 {
        self.keys.median / self.plain.median
    }
}

/// Measures the encoded case at the guard.
///
/// Both sides describe the same eight thousand values. One is a run of `i32` keys into a dictionary
/// of two hundred and fifty six, and the other is the `i64` values themselves. The encoded form is
/// half the bytes and costs far more to check, because a key is a position into something else and
/// every one of them has to be looked at, while the plain column is a single multiplication.
fn encoded(count: usize) -> Encoded {
    let keys: Vec<u8> = (0..ROWS)
        .map(|row| i32::try_from(row % DICTIONARY).expect("256 fits"))
        .flat_map(i32::to_le_bytes)
        .collect();

    let schema = Schema::new(vec![Field::new("v", DataType::Int64, false)]);
    let values: Vec<u8> = (0..ROWS)
        .flat_map(|v| i64::try_from(v).expect("8192 fits").to_le_bytes())
        .collect();
    let nodes = [Node {
        length: ROWS,
        null_count: 0,
    }];
    let buffers = vec![Vec::new(), values];

    let keys_summary = summarise(&samples(count, || {
        black_box(iris_guard::check_dictionary(
            black_box(&keys),
            &DataType::Int32,
            ROWS,
            DICTIONARY,
            "v",
        ))
        .expect("the keys are all inside the dictionary");
    }));

    let plain_summary = summarise(&samples(count, || {
        black_box(iris_guard::check(
            black_box(&schema),
            ROWS,
            &nodes,
            &buffers,
        ))
        .expect("the plain column is sound");
    }));

    Encoded {
        keys: keys_summary,
        plain: plain_summary,
    }
}

/// Which of the three committed outcomes this measurement lands in.
fn verdict(worst: f64) -> &'static str {
    if worst < SETTLED_GATE {
        "under 5 percent: the guard stays on and there is nothing further to say"
    } else if worst < DESIGN_GATE {
        "between 5 and 15 percent: the guard stays on and digest pinning is the documented path \
         for a host that needs the difference"
    } else {
        "over 15 percent: the guard stays on and the cost is a design problem rather than a check \
         to remove"
    }
}

fn human(target: &str, measured: &[Measured], encoded: &Encoded) {
    println!("iris-guard cost on {target}");
    println!();
    println!("The guard as a share of getting a batch into Arrow. Assembly is the tightest");
    println!("denominator there is, so these are upper bounds on the share of a scan.");
    println!();
    println!(
        "{:<16} {:>12} {:>12} {:>8}  why it is here",
        "shape", "guard ns", "build ns", "share"
    );
    for m in measured {
        println!(
            "{:<16} {:>12.0} {:>12.0} {:>7.2}%  {}",
            m.name,
            m.guard.median,
            m.build.median,
            m.share() * 100.0,
            m.why
        );
    }
    println!();
    for m in measured {
        println!(
            "  {:<16} guard 95% [{:.0}, {:.0}] n={}, build 95% [{:.0}, {:.0}] n={}",
            m.name, m.guard.lo, m.guard.hi, m.guard.n, m.build.lo, m.build.hi, m.build.n
        );
    }
    println!();
    println!("Encoded against plain, at the guard, for the same eight thousand values. The guard");
    println!("charges for indirection rather than for bytes, so the smaller one costs more:");
    println!(
        "  dictionary keys {:.0} ns, the plain column {:.0} ns, ratio {:.2}",
        encoded.keys.median,
        encoded.plain.median,
        encoded.ratio()
    );
    println!();

    let worst = measured.iter().map(Measured::share).fold(0.0, f64::max);
    println!("Worst share: {:.2} percent", worst * 100.0);
    println!("Rule: {}", verdict(worst));
}

fn summary_json(label: &str, s: &Summary) -> String {
    format!(
        "\"{label}\":{{\"median_ns\":{:.1},\"lo_ns\":{:.1},\"hi_ns\":{:.1},\"n\":{}}}",
        s.median, s.lo, s.hi, s.n
    )
}

fn json(target: &str, measured: &[Measured], encoded: &Encoded) {
    let worst = measured.iter().map(Measured::share).fold(0.0, f64::max);
    let shapes: Vec<String> = measured
        .iter()
        .map(|m| {
            format!(
                "{{\"shape\":\"{}\",{},{},\"share\":{:.6}}}",
                m.name,
                summary_json("guard", &m.guard),
                summary_json("build", &m.build),
                m.share()
            )
        })
        .collect();

    println!(
        "{{\"probe\":\"guard_cost\",\"target\":\"{target}\",\"rows\":{ROWS},\
         \"shapes\":[{}],\
         \"encoded\":{{{},{},\"ratio\":{:.6}}},\
         \"worst_share\":{:.6},\"verdict\":\"{}\"}}",
        shapes.join(","),
        summary_json("keys", &encoded.keys),
        summary_json("plain", &encoded.plain),
        encoded.ratio(),
        worst,
        verdict(worst)
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
    let count = parse_arg(&args, "--samples", 200usize).max(8);
    let target = format!("{}-{}", env::consts::OS, env::consts::ARCH);

    let shapes = [plain(), nullable(), strings()];
    let measured: Vec<Measured> = shapes.iter().map(|s| measure(s, count)).collect();
    let encoded = encoded(count);

    if args.iter().any(|a| a == "--json") {
        json(&target, &measured, &encoded);
    } else {
        human(&target, &measured, &encoded);
    }
}
