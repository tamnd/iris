//! The M0 probe.
//!
//! Two measurements decide whether the architecture in `docs/ROADMAP.md` is worth writing, and this
//! is the harness that takes them. It is deliberately an example rather than part of the library,
//! because it measures a design question and not a feature.
//!
//! Run it with `cargo run --release -p iris-vm --example m0_probe`. Add `--json` for a machine
//! readable object suitable for handing to iris-bench.
//!
//! Measurement one is the round trip cost of a `require_range` style host call. The whole I/O model
//! rests on the decoder asking the host for byte ranges instead of being handed a mapped file, so if
//! a host call is expensive then the inversion is unaffordable and the design has to change to a
//! shared window descriptor before anything else is written.
//!
//! Measurement two is what the sliding window costs on a scan that is already resident. The window
//! is what removes the four gibibyte ceiling and what makes object storage reachable, and the claim
//! being tested is that supporting remote storage does not tax the local path.
//!
//! Both are reported as a median with a bootstrap confidence interval and a sample size, because a
//! bare mean of a timing distribution is not a number anyone should act on.

use std::env;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use wasmtime::{Caller, Engine, Instance, Linker, Memory, Module, Store};

/// A WebAssembly page, which is the unit `Memory::grow` counts in.
const PAGE: usize = 65_536;

/// Cost of a single host call above which the range inversion is unaffordable.
const HOSTCALL_GATE_NS: f64 = 100.0;

/// Fraction of the flat scan above which windowing is taxing the local path.
const WINDOW_GATE: f64 = 0.03;

/// A loop that calls an imported host function, and the same loop without the call.
///
/// The difference between the two is the thing being measured. Both loops do the same arithmetic and
/// the same number of iterations, so what is left over is the call.
const HOSTCALL_WAT: &str = r#"
(module
  (import "iris" "require_range" (func $rr (param i64 i64) (result i32)))

  (func (export "run_call") (param $n i64) (result i32)
    (local $i i64) (local $acc i32)
    (block $done
      (loop $l
        (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
        (local.set $acc
          (i32.add (local.get $acc)
                   (call $rr (local.get $i) (i64.const 4096))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $l)))
    (local.get $acc))

  (func (export "run_nop") (param $n i64) (result i32)
    (local $i i64) (local $acc i32)
    (block $done
      (loop $l
        (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
        (local.set $acc (i32.add (local.get $acc) (i32.const 1)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $l)))
    (local.get $acc))
)
"#;

/// A scan loop in two shapes: flat over the whole buffer, and chunked with a host call per chunk.
///
/// `sum_chunked` takes a stride so the same code can express two different situations. With the
/// stride equal to the window it walks forward through a large mapped region, which is the shape
/// that isolates what the windowed control flow costs. With the stride at zero it reads the same
/// window every time, which is the shape where the host has to refill the window between chunks.
const SCAN_WAT: &str = r#"
(module
  (import "iris" "slide" (func $slide (param i32) (result i32)))
  (memory (export "mem") 1)

  (func (export "sum_flat") (param $len i32) (result i64)
    (local $i i32) (local $acc i64)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $acc (i64.add (local.get $acc) (i64.load (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 8)))
        (br $l)))
    (local.get $acc))

  (func (export "sum_chunked") (param $chunks i32) (param $win i32) (param $stride i32) (result i64)
    (local $c i32) (local $i i32) (local $base i32) (local $acc i64)
    (block $outer_done
      (loop $outer
        (br_if $outer_done (i32.ge_u (local.get $c) (local.get $chunks)))
        (drop (call $slide (local.get $c)))
        (local.set $base (i32.mul (local.get $c) (local.get $stride)))
        (local.set $i (i32.const 0))
        (block $inner_done
          (loop $inner
            (br_if $inner_done (i32.ge_u (local.get $i) (local.get $win)))
            (local.set $acc
              (i64.add (local.get $acc)
                       (i64.load (i32.add (local.get $base) (local.get $i)))))
            (local.set $i (i32.add (local.get $i) (i32.const 8)))
            (br $inner)))
        (local.set $c (i32.add (local.get $c) (i32.const 1)))
        (br $outer)))
    (local.get $acc))
)
"#;

/// Everything the host side of the probe needs in order to answer a call.
struct ProbeState {
    calls: u64,
    resident_lo: u64,
    resident_hi: u64,
    memory: Option<Memory>,
    source: Arc<[u8]>,
    window: usize,
    refill: bool,
}

impl ProbeState {
    fn new() -> Self {
        Self {
            calls: 0,
            resident_lo: 0,
            resident_hi: u64::MAX,
            memory: None,
            source: Arc::from(Vec::new()),
            window: 0,
            refill: false,
        }
    }
}

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

impl Summary {
    /// The half width of the interval as a fraction of the median, which is the number that decides
    /// whether more repetitions are worth taking.
    fn relative_width(&self) -> f64 {
        if self.median == 0.0 {
            return f64::INFINITY;
        }
        (self.hi - self.lo) / (2.0 * self.median)
    }
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
/// Ten thousand resamples is more than the interval needs and cheap enough not to bother tuning. The
/// generator is a fixed seed xorshift so that the same samples always produce the same interval,
/// which matters when someone is trying to work out whether a number really moved.
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

/// Which host function shape is bound to the import.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallShape {
    /// A closure that ignores its arguments. The floor for what a host call can cost.
    Plain,
    /// A closure that takes the caller, reads store data and answers whether the range is resident.
    /// This is the shape a real `require_range` has to be, so it is the number that matters.
    Resident,
}

fn build_hostcall(
    engine: &Engine,
    shape: CallShape,
) -> wasmtime::Result<(Store<ProbeState>, Instance)> {
    let module = Module::new(engine, HOSTCALL_WAT)?;
    let mut store = Store::new(engine, ProbeState::new());
    let mut linker = Linker::new(engine);
    match shape {
        CallShape::Plain => {
            linker.func_wrap("iris", "require_range", |_off: i64, _len: i64| -> i32 { 1 })?;
        }
        CallShape::Resident => {
            linker.func_wrap(
                "iris",
                "require_range",
                |mut caller: Caller<'_, ProbeState>, off: i64, len: i64| -> i32 {
                    let state = caller.data_mut();
                    state.calls += 1;
                    let start = off.cast_unsigned();
                    let end = start.saturating_add(len.cast_unsigned());
                    i32::from(start >= state.resident_lo && end <= state.resident_hi)
                },
            )?;
        }
    }
    let instance = linker.instantiate(&mut store, &module)?;
    Ok((store, instance))
}

/// Time one call and return how long it took, in nanoseconds.
fn timed<T>(f: impl FnOnce() -> wasmtime::Result<T>) -> wasmtime::Result<f64> {
    let start = Instant::now();
    let observed = f()?;
    let elapsed = start.elapsed().as_secs_f64() * 1e9;
    std::hint::black_box(observed);
    Ok(elapsed)
}

struct HostcallResult {
    nop: Summary,
    plain: Summary,
    resident: Summary,
    per_call: f64,
}

/// Take all three host call configurations, one sample of each per round.
///
/// The configurations are interleaved rather than run in blocks. Run them in blocks and the first
/// one is measured while the processor is still ramping up its clock, which on this hardware is
/// worth more than the effect being measured. Interleaving costs nothing and removes it.
fn measure_hostcall(
    engine: &Engine,
    iters: i64,
    samples: usize,
) -> wasmtime::Result<HostcallResult> {
    let (mut plain_store, plain_instance) = build_hostcall(engine, CallShape::Plain)?;
    let (mut resident_store, resident_instance) = build_hostcall(engine, CallShape::Resident)?;

    let nop_fn = plain_instance.get_typed_func::<i64, i32>(&mut plain_store, "run_nop")?;
    let plain_fn = plain_instance.get_typed_func::<i64, i32>(&mut plain_store, "run_call")?;
    let resident_fn =
        resident_instance.get_typed_func::<i64, i32>(&mut resident_store, "run_call")?;

    for _ in 0..5 {
        nop_fn.call(&mut plain_store, iters)?;
        plain_fn.call(&mut plain_store, iters)?;
        resident_fn.call(&mut resident_store, iters)?;
    }

    let per = f64::from(i32::try_from(iters)?);
    let mut nop = Vec::with_capacity(samples);
    let mut plain = Vec::with_capacity(samples);
    let mut resident = Vec::with_capacity(samples);
    for _ in 0..samples {
        nop.push(timed(|| nop_fn.call(&mut plain_store, iters))? / per);
        plain.push(timed(|| plain_fn.call(&mut plain_store, iters))? / per);
        resident.push(timed(|| resident_fn.call(&mut resident_store, iters))? / per);
    }

    let nop = summarise(&nop);
    let plain = summarise(&plain);
    let resident = summarise(&resident);
    let per_call = resident.median - nop.median;
    Ok(HostcallResult {
        nop,
        plain,
        resident,
        per_call,
    })
}

/// Write a file of pseudorandom bytes, read it back, and hand back the bytes.
///
/// The measurement is about a file that is already in the page cache. Anything else is measuring the
/// storage device, which is a different question and belongs in iris-bench.
fn resident_source(bytes: usize) -> wasmtime::Result<(Arc<[u8]>, PathBuf)> {
    let path = env::temp_dir().join(format!("iris-m0-{}.bin", std::process::id()));
    let mut file = File::create(&path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut written = 0;
    while written < bytes {
        for chunk in buf.as_chunks_mut::<8>().0 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        let take = buf.len().min(bytes - written);
        file.write_all(&buf[..take])?;
        written += take;
    }
    file.sync_all()?;
    drop(file);

    let data = std::fs::read(&path)?;
    let checksum = data
        .iter()
        .fold(0u64, |acc, b| acc.wrapping_add(u64::from(*b)));
    std::hint::black_box(checksum);
    Ok((Arc::from(data), path))
}

struct WindowResult {
    flat: Summary,
    windowed: Summary,
    refill: Summary,
    overhead: f64,
    refill_overhead: f64,
    bytes: usize,
    window: usize,
}

fn measure_window(
    engine: &Engine,
    bytes: usize,
    window: usize,
    samples: usize,
) -> wasmtime::Result<WindowResult> {
    let (source, path) = resident_source(bytes)?;

    let module = Module::new(engine, SCAN_WAT)?;
    let mut store = Store::new(engine, ProbeState::new());
    store.data_mut().source = Arc::clone(&source);
    store.data_mut().window = window;

    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "iris",
        "slide",
        |mut caller: Caller<'_, ProbeState>, chunk: i32| -> i32 {
            let state = caller.data_mut();
            state.calls += 1;
            if !state.refill {
                return 0;
            }
            let window = state.window;
            let Some(memory) = state.memory else {
                return 0;
            };
            let source = Arc::clone(&state.source);
            let offset = usize::try_from(chunk).unwrap_or(0).saturating_mul(window);
            let end = offset.saturating_add(window).min(source.len());
            if offset >= end {
                return 0;
            }
            let len = end - offset;
            memory.data_mut(&mut caller)[..len].copy_from_slice(&source[offset..end]);
            1
        },
    )?;

    let instance = linker.instantiate(&mut store, &module)?;
    let memory = instance
        .get_memory(&mut store, "mem")
        .ok_or_else(|| wasmtime::format_err!("the scan module did not export its memory"))?;

    let want_pages = bytes.div_ceil(PAGE);
    let have_pages = usize::try_from(memory.size(&store)).unwrap_or(0);
    if want_pages > have_pages {
        memory.grow(&mut store, u64::try_from(want_pages - have_pages)?)?;
    }
    memory.data_mut(&mut store)[..bytes].copy_from_slice(&source);
    store.data_mut().memory = Some(memory);

    let len = i32::try_from(bytes)?;
    let win = i32::try_from(window)?;
    let chunks = i32::try_from(bytes / window)?;

    let flat_fn = instance.get_typed_func::<i32, i64>(&mut store, "sum_flat")?;
    let chunked_fn = instance.get_typed_func::<(i32, i32, i32), i64>(&mut store, "sum_chunked")?;

    // All three configurations read the same bytes, so all three must produce the same sum. This is
    // cheap and it catches the mistake that would otherwise make the whole measurement meaningless,
    // which is a windowed configuration that is fast because it is quietly reading less data.
    store.data_mut().refill = false;
    let flat_sum = flat_fn.call(&mut store, len)?;
    let windowed_sum = chunked_fn.call(&mut store, (chunks, win, win))?;
    store.data_mut().refill = true;
    let refill_sum = chunked_fn.call(&mut store, (chunks, win, 0))?;
    if flat_sum != windowed_sum || flat_sum != refill_sum {
        wasmtime::bail!(
            "the three configurations disagree: flat {flat_sum}, windowed {windowed_sum}, refill {refill_sum}"
        );
    }

    // The three configurations are interleaved, one sample of each per round, for the reason given
    // on `measure_hostcall`. Blocked measurement here produced a windowed scan that looked eleven
    // percent faster than a flat one, which was the processor ramping up and not the code.
    //
    // Configuration one is a flat pass over the whole buffer, which is what mapping the entire
    // dataset into guest memory looks like from inside the decode loop.
    //
    // Configuration two walks the same bytes in the same order but through the windowed control
    // flow, with a host call per chunk that does nothing. The difference from configuration one is
    // the cost of the abstraction and not the cost of moving data, so this is the gate.
    //
    // Configuration three does a real refill, where the host copies the next window into guest
    // memory between chunks. That is the naive implementation of windowing, measured separately so
    // that the cost of copying is visible rather than folded into the gate.
    for _ in 0..3 {
        store.data_mut().refill = false;
        flat_fn.call(&mut store, len)?;
        chunked_fn.call(&mut store, (chunks, win, win))?;
        store.data_mut().refill = true;
        chunked_fn.call(&mut store, (chunks, win, 0))?;
    }

    let mut flat = Vec::with_capacity(samples);
    let mut windowed = Vec::with_capacity(samples);
    let mut refill = Vec::with_capacity(samples);
    for _ in 0..samples {
        store.data_mut().refill = false;
        flat.push(timed(|| flat_fn.call(&mut store, len))?);
        windowed.push(timed(|| chunked_fn.call(&mut store, (chunks, win, win)))?);
        store.data_mut().refill = true;
        refill.push(timed(|| chunked_fn.call(&mut store, (chunks, win, 0)))?);
    }

    drop(std::fs::remove_file(&path));

    let flat = summarise(&flat);
    let windowed = summarise(&windowed);
    let refill = summarise(&refill);
    let overhead = (windowed.median - flat.median) / flat.median;
    let refill_overhead = (refill.median - flat.median) / flat.median;

    Ok(WindowResult {
        flat,
        windowed,
        refill,
        overhead,
        refill_overhead,
        bytes,
        window,
    })
}

fn parse_arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn verdict(pass: bool) -> &'static str {
    if pass { "pass" } else { "FAIL" }
}

fn human(target: &str, hostcall: &HostcallResult, window: &WindowResult) {
    let hc = hostcall;
    println!("iris M0 probe");
    println!("target {target}");
    println!();
    println!("Host call round trip");
    println!(
        "  loop without a call    {:>8.2} ns per iteration  ({:.2} to {:.2}, n = {})",
        hc.nop.median, hc.nop.lo, hc.nop.hi, hc.nop.n
    );
    println!(
        "  loop with a bare call  {:>8.2} ns per iteration  ({:.2} to {:.2}, n = {})",
        hc.plain.median, hc.plain.lo, hc.plain.hi, hc.plain.n
    );
    println!(
        "  loop with a real call  {:>8.2} ns per iteration  ({:.2} to {:.2}, n = {})",
        hc.resident.median, hc.resident.lo, hc.resident.hi, hc.resident.n
    );
    println!("  cost of one host call  {:>8.2} ns", hc.per_call);
    println!(
        "  gate is {HOSTCALL_GATE_NS} ns per call: {}",
        verdict(hc.per_call < HOSTCALL_GATE_NS)
    );
    println!();
    println!(
        "Sliding window over a resident {} MiB buffer, {} MiB window",
        window.bytes >> 20,
        window.window >> 20
    );
    println!(
        "  flat scan              {:>8.2} ms  ({:.2} to {:.2}, n = {})",
        window.flat.median / 1e6,
        window.flat.lo / 1e6,
        window.flat.hi / 1e6,
        window.flat.n
    );
    println!(
        "  windowed, no refill    {:>8.2} ms  ({:.2} to {:.2}, n = {})",
        window.windowed.median / 1e6,
        window.windowed.lo / 1e6,
        window.windowed.hi / 1e6,
        window.windowed.n
    );
    println!(
        "  windowed, host refill  {:>8.2} ms  ({:.2} to {:.2}, n = {})",
        window.refill.median / 1e6,
        window.refill.lo / 1e6,
        window.refill.hi / 1e6,
        window.refill.n
    );
    println!(
        "  abstraction overhead   {:>8.2} %",
        window.overhead * 100.0
    );
    println!(
        "  overhead with a copy   {:>8.2} %",
        window.refill_overhead * 100.0
    );
    println!(
        "  gate is 3 percent on the abstraction: {}",
        verdict(window.overhead < WINDOW_GATE)
    );
    println!();
    println!("Interval half widths as a fraction of the median:");
    println!(
        "  host call {:.4}, flat scan {:.4}, windowed {:.4}",
        hc.resident.relative_width(),
        window.flat.relative_width(),
        window.windowed.relative_width()
    );
    println!();
    println!(
        "These numbers are not publishable on their own. Publishing happens in iris-bench, on"
    );
    println!("a machine that passed its eligibility gates, against a registered claim.");
}

fn summary_json(label: &str, s: &Summary) -> String {
    format!(
        "\"{label}\":{{\"median_ns\":{:.4},\"ci_lo_ns\":{:.4},\"ci_hi_ns\":{:.4},\"samples\":{}}}",
        s.median, s.lo, s.hi, s.n
    )
}

fn json(target: &str, hostcall: &HostcallResult, window: &WindowResult) {
    let hostcall_fields = [
        summary_json("nop", &hostcall.nop),
        summary_json("plain", &hostcall.plain),
        summary_json("resident", &hostcall.resident),
        format!("\"per_call_ns\":{:.4}", hostcall.per_call),
        format!("\"gate_ns\":{HOSTCALL_GATE_NS}"),
        format!("\"pass\":{}", hostcall.per_call < HOSTCALL_GATE_NS),
    ]
    .join(",");
    let window_fields = [
        summary_json("flat", &window.flat),
        summary_json("windowed", &window.windowed),
        summary_json("refill", &window.refill),
        format!("\"bytes\":{}", window.bytes),
        format!("\"window_bytes\":{}", window.window),
        format!("\"abstraction_overhead\":{:.6}", window.overhead),
        format!("\"refill_overhead\":{:.6}", window.refill_overhead),
        format!("\"gate_overhead\":{WINDOW_GATE}"),
        format!("\"pass\":{}", window.overhead < WINDOW_GATE),
    ]
    .join(",");
    println!(
        "{{\"probe\":\"iris-m0\",\"methodology\":1,\"target\":\"{target}\",\
         \"hostcall\":{{{hostcall_fields}}},\"window\":{{{window_fields}}}}}"
    );
}

fn main() -> wasmtime::Result<()> {
    let args: Vec<String> = env::args().collect();
    let as_json = args.iter().any(|a| a == "--json");
    let iters: i64 = parse_arg(&args, "--iters", 1_000_000);
    let samples: usize = parse_arg(&args, "--samples", 25);
    let mib: usize = parse_arg(&args, "--mib", 256);
    let window_mib: usize = parse_arg(&args, "--window-mib", 16);
    let target = format!("{}-{}", env::consts::ARCH, env::consts::OS);

    let engine = Engine::default();
    let hostcall = measure_hostcall(&engine, iters, samples)?;
    let window = measure_window(&engine, mib << 20, window_mib << 20, samples)?;

    if as_json {
        json(&target, &hostcall, &window);
    } else {
        human(&target, &hostcall, &window);
    }
    Ok(())
}
