//! A decode job that is put down on one thread and picked up on another, once per suspension.
//!
//! The gate this holds is the M6 one in `docs/ROADMAP.md`, that a decode job is `Send` structurally
//! rather than by assertion. The compile time half of that is in `iris-vm/src/lib.rs` and the half
//! that says no to the two usual shortcuts is in `ci/discipline.py`. This is the half that runs.
//!
//! The prior art in this area declares `unsafe impl Send` on its job type and then checks at run
//! time that the thread it is on is the thread it started on. That combination is correct under a
//! harness that pins work to threads, which is what the paper measured, and it is unsound under any
//! executor that moves a task after it parks. `DataFusion` and Tokio both move tasks after they park.
//! So the property worth testing is not that a job survives being polled from a pool, it is that a
//! job survives being polled from a thread that has never seen it before, which is the case the
//! assertion was there to forbid.
//!
//! Every poll here happens on a thread spawned for that poll and joined before the next one starts.
//! Nothing has to ask which thread it is on for that to mean something: a thread that was created
//! after the previous poll returned cannot be the thread the previous poll ran on. So the guest's
//! stack is resumed on a first time thread [`SUSPENSIONS`] times, and the guest is the thing that
//! says whether that worked, because it adds up every byte it read in a WebAssembly local and traps
//! rather than returning if the total is wrong.
//!
//! A local rather than a global, for the reason `tests/resume.rs` gives: a global would survive a
//! host that threw the call away and started it again, so a test built on one would pass against an
//! implementation that replays.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use iris_source::{Fetch, RangeSource, SourceError, Traffic, bounds};
use iris_vm::{Decoder, Progress, Running, Vm};

/// How many eight byte values the decoder reads, one range each.
const VALUES: u64 = 256;

/// How many bytes that is, which is what the module below counts up to.
const BYTES: u64 = VALUES * 8;

/// How many times the scan stops, which is once per range because the source refuses each one once.
const SUSPENSIONS: u64 = VALUES;

/// The sum of every value in the corpus, which is the sum of the integers below [`VALUES`].
const CHECKSUM: u64 = VALUES * (VALUES - 1) / 2;

/// What the host calls the module under test.
const IDENTITY: &str = "blake3:2c8f11";

/// A decoder that reads the whole source eight bytes at a time and checks its own arithmetic.
///
/// The running total is a local, so it lives on the guest's stack and nowhere else. A host that lost
/// the stack has no way to make this module report success.
const ADDS_IT_UP: &str = r#"
(module
  (import "iris" "require_range" (func $require_range (param i64 i32 i32) (result i32)))
  (memory (export "memory") 1)

  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))

  (func (export "iris_scan") (result i64)
    (local $at i64)
    (local $sum i64)
    (block $done
      (loop $next
        (br_if $done (i64.ge_u (local.get $at) (i64.const 2048)))
        (if (i32.ne
              (call $require_range (local.get $at) (i32.const 8) (i32.const 0))
              (i32.const 0))
          (then (unreachable)))
        (local.set $sum (i64.add (local.get $sum) (i64.load (i32.const 0))))
        (local.set $at (i64.add (local.get $at) (i64.const 8)))
        (br $next)))
    (if (i64.ne (local.get $sum) (i64.const 32640))
      (then (unreachable)))
    (i64.const 0))
)
"#;

/// Eight byte little endian integers counting up from zero.
fn corpus() -> Vec<u8> {
    (0..VALUES).flat_map(u64::to_le_bytes).collect()
}

/// A source that refuses each distinct range once and serves it the second time it is asked.
///
/// One refusal per range rather than a fixed budget across the scan, so the number of suspensions is
/// the number of ranges and the test can assert on it.
struct Once {
    bytes: Vec<u8>,
    asked: Option<(u64, usize)>,
    refused: bool,
    refusals: Arc<AtomicU64>,
    served: Traffic,
}

impl Once {
    fn new(bytes: Vec<u8>) -> (Self, Arc<AtomicU64>) {
        let refusals = Arc::new(AtomicU64::new(0));
        let source = Self {
            bytes,
            asked: None,
            refused: false,
            refusals: Arc::clone(&refusals),
            served: Traffic::NONE,
        };
        (source, refusals)
    }
}

impl RangeSource for Once {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).expect("a corpus this small fits in a u64")
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;

        if self.asked != Some((at, len)) {
            self.asked = Some((at, len));
            self.refused = false;
        }

        if !self.refused {
            self.refused = true;
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Ok(Fetch::Pending);
        }

        self.served.requests += 1;
        self.served.bytes += u64::try_from(len).expect("a request this small fits in a u64");
        let start = usize::try_from(at).expect("a corpus this small is addressable");
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        self.served
    }
}

impl std::fmt::Debug for Once {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Once").finish_non_exhaustive()
    }
}

/// Instantiates the module and gives it a source to pull from.
fn decoder(source: Box<dyn RangeSource + Send>) -> Decoder {
    let program = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .compile(ADDS_IT_UP.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("the module instantiates");
    decoder.attach(source);
    decoder
}

/// Polls a job once, on a thread that has just been made for it, and hands the job back.
///
/// The job goes into the thread and comes out of it again, so the compiler is being asked the same
/// question the gate asks: may this value cross a thread boundary. A borrow would not have asked it.
fn poll_elsewhere<T: Send>(job: Running<'_, T>) -> (Running<'_, T>, Progress<T>) {
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut job = job;
                let progress = job.poll().expect("the scan does not fail");
                (job, progress)
            })
            .join()
            .expect("the polling thread did not panic")
    })
}

#[test]
fn a_suspended_scan_resumes_on_a_thread_that_has_never_seen_it() {
    let (source, refusals) = Once::new(corpus());
    let mut decoder = decoder(Box::new(source));

    // The job borrows the decoder, so it is finished with inside this block and the decoder is
    // available again afterwards.
    let stops = {
        let mut job = decoder.scan(&[]);
        let mut stops: u64 = 0;
        loop {
            let (back, progress) = poll_elsewhere(job);
            job = back;
            match progress {
                Progress::Done(batches) => {
                    assert!(
                        batches.is_empty(),
                        "this module reads and adds up, it does not emit"
                    );
                    break stops;
                }
                Progress::Suspended => stops += 1,
            }
        }
    };

    assert_eq!(
        stops, SUSPENSIONS,
        "the source refuses every range once, so the scan stops once per range"
    );
    assert_eq!(
        refusals.load(Ordering::Relaxed),
        VALUES,
        "one refusal per range and no replays, because a replayed range would ask again"
    );

    // The guest traps rather than returning when its total is wrong, so getting here at all is the
    // assertion. This is the number it checked itself against, written down where a reader can see
    // what the module was proving.
    assert_eq!(CHECKSUM, 32_640);
    assert_eq!(BYTES, 2_048);

    // Two served calls per range and not one. Establishing that a range is ready and taking the
    // bytes out of it are two calls, because the borrow the readiness loop takes cannot be seen to
    // end by a borrow checker that has not been told the iteration returned. `iris-source` does the
    // same thing in `read_blocking` for the same reason. What matters here is that the number does
    // not depend on how the polling was spread across threads.
    let source = decoder.detach().expect("the source is still attached");
    assert_eq!(
        source.traffic(),
        Traffic {
            requests: VALUES * 2,
            bytes: BYTES * 2,
        },
        "every range was served the same way it would have been on one thread"
    );
}

#[test]
fn a_decoder_between_scans_belongs_to_whoever_has_it() {
    let (source, _) = Once::new(corpus());
    let decoder = decoder(Box::new(source));

    // Three scans on three threads, with the decoder moved rather than shared. A pool that hands
    // the same decoder to a different worker for each of a query's batches is doing this, and a
    // decoder that remembered where it was instantiated would refuse the second one.
    let mut decoder = decoder;
    for _ in 0..3 {
        decoder = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let mut owned = decoder;
                    owned.scan(&[]).wait().expect("the scan finishes");
                    owned
                })
                .join()
                .expect("the scanning thread did not panic")
        });
    }
}
