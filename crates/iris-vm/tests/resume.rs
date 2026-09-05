//! Suspension: a decoder that asks for bytes it does not have is stopped and started again, and it
//! does not notice.
//!
//! The gate this test holds is the one in `docs/ROADMAP.md` for M4, that the resumable path works
//! from a single threaded host that never blocks. The interesting half of it is the word
//! unbounded. One suspension is easy to get right by accident. What is being checked here is that
//! the four thousandth resumption costs what the first one did and lands the guest on the same
//! instruction, and the way it is checked is that the decoder adds up every byte it read and refuses
//! to return if the total is not the one it should be.
//!
//! The checksum lives in a WebAssembly local rather than a global on purpose. A global would survive
//! a host that unwound the call and started it again, so a test built on one would pass against an
//! implementation that replays. A local lives on the guest's stack, and the only way it is still
//! there after four thousand suspensions is if the stack was never thrown away.
//!
//! The module is hand written text rather than a decoder built from the SDK, because what is under
//! test is the host. The SDK's own side of this is exercised where the SDK is.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the corpus is thirty two kilobytes and every offset here has already been bounds \
              checked against it"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use iris_source::{Fetch, RangeSource, SourceError, Traffic, bounds};
use iris_vm::{Decoder, Error, Progress, Vm};

/// How many eight byte values the decoder reads, one range each.
///
/// Large enough that nothing about the result could be explained by a fixed number of retries
/// somewhere, and small enough that the test is over in well under a second.
const VALUES: u64 = 4096;

/// The sum of every value in the corpus, which is the sum of the integers below [`VALUES`].
const CHECKSUM: u64 = VALUES * (VALUES - 1) / 2;

/// What the host calls the module under test.
const IDENTITY: &str = "blake3:9a3d07";

/// A decoder that reads the whole source eight bytes at a time and checks its own arithmetic.
///
/// It traps rather than returning if the total is wrong, so a host that lost the guest's place has
/// no way to make this module report success.
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
        (br_if $done (i64.ge_u (local.get $at) (i64.const 32768)))
        (if (i32.ne
              (call $require_range (local.get $at) (i32.const 8) (i32.const 0))
              (i32.const 0))
          (then (unreachable)))
        (local.set $sum (i64.add (local.get $sum) (i64.load (i32.const 0))))
        (local.set $at (i64.add (local.get $at) (i64.const 8)))
        (br $next)))
    (if (i64.ne (local.get $sum) (i64.const 8386560))
      (then (unreachable)))
    (i64.const 0))
)
"#;

/// A decoder that asks for a range past the end and expects to be told so rather than stopped.
///
/// The status codes that mean the decoder asked for the wrong thing are answers, not failures, so
/// this module carries on afterwards and reads a range that does exist.
const ASKS_TOO_FAR: &str = r#"
(module
  (import "iris" "require_range" (func $require_range (param i64 i32 i32) (result i32)))
  (memory (export "memory") 1)

  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))

  (func (export "iris_scan") (result i64)
    (if (i32.ne
          (call $require_range (i64.const 32760) (i32.const 16) (i32.const 0))
          (i32.const 1))
      (then (unreachable)))
    (if (i32.ne
          (call $require_range (i64.const 32760) (i32.const 8) (i32.const 0))
          (i32.const 0))
      (then (unreachable)))
    (if (i64.ne (i64.load (i32.const 0)) (i64.const 4095))
      (then (unreachable)))
    (i64.const 0))
)
"#;

/// A decoder that asks for one range and does nothing else.
const ASKS_ONCE: &str = r#"
(module
  (import "iris" "require_range" (func $require_range (param i64 i32 i32) (result i32)))
  (memory (export "memory") 1)

  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))

  (func (export "iris_scan") (result i64)
    (drop (call $require_range (i64.const 0) (i32.const 8) (i32.const 0)))
    (i64.const 0))
)
"#;

/// Eight byte little endian integers counting up from zero.
fn corpus() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VALUES as usize * 8);
    for value in 0..VALUES {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// A source that says no a fixed number of times before serving anything.
///
/// This is the thing under test made visible. A real object store misses when a block is not cached
/// and hits when it is, so the number of suspensions in a real scan depends on the data. Here it is
/// a constant, which is what lets the test assert on it.
struct Reluctant {
    bytes: Vec<u8>,
    /// How many times each distinct range is refused before it is served.
    misses: u32,
    /// How many refusals the current range has left.
    left: u32,
    /// Which range the count belongs to, so that asking for a different one starts again.
    asked: Option<(u64, usize)>,
    /// Every refusal this source has made, readable after it has been handed to a decoder.
    refusals: Arc<AtomicU64>,
    /// What it has served, counted the way a source over a network would count it.
    served: Traffic,
}

impl Reluctant {
    fn new(bytes: Vec<u8>, misses: u32) -> (Self, Arc<AtomicU64>) {
        let refusals = Arc::new(AtomicU64::new(0));
        let source = Self {
            bytes,
            misses,
            left: 0,
            asked: None,
            refusals: Arc::clone(&refusals),
            served: Traffic::NONE,
        };
        (source, refusals)
    }
}

impl RangeSource for Reluctant {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;

        if self.asked != Some((at, len)) {
            self.asked = Some((at, len));
            self.left = self.misses;
        }

        if self.left > 0 {
            self.left -= 1;
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Ok(Fetch::Pending);
        }

        self.served.requests += 1;
        self.served.bytes += len as u64;
        let start = at as usize;
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        // Counted when the range is served rather than when it is asked for, which is what a real
        // source does: the refusals above are this source saying the bytes are not here yet, and
        // nothing has crossed anything at that point.
        self.served
    }
}

/// Instantiates a module and gives it a source to pull from.
fn decoder(text: &str, source: Box<dyn RangeSource + Send>) -> Decoder {
    let program = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .compile(text.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("the module instantiates");
    decoder.attach(source);
    decoder
}

/// Drives a scan to the end the way a host that never blocks would, counting the stops.
fn drive(decoder: &mut Decoder) -> (usize, Result<(), Error>) {
    let mut running = decoder.scan(&[]);
    let mut stops = 0;
    loop {
        match running.poll() {
            Ok(Progress::Done(_)) => return (stops, Ok(())),
            Ok(Progress::Suspended) => stops += 1,
            Err(err) => return (stops, Err(err)),
        }
    }
}

#[test]
fn a_decode_survives_a_miss_on_every_range() {
    let (source, refusals) = Reluctant::new(corpus(), 1);
    let mut decoder = decoder(ADDS_IT_UP, Box::new(source));

    let (stops, outcome) = drive(&mut decoder);
    outcome.expect("the decoder adds up to the right total, so it returns rather than trapping");

    // One miss per range, so one suspension per range, and the decoder still got the answer. The
    // module traps unless its running total is right, which it cannot be unless the local it was
    // accumulating into survived all four thousand of these.
    assert_eq!(refusals.load(Ordering::Relaxed), VALUES);
    assert_eq!(stops as u64, VALUES);
    assert_eq!(CHECKSUM, 8_386_560);
}

#[test]
fn missing_repeatedly_on_the_same_range_costs_nothing_but_time() {
    // Three misses per range rather than one, which is a source that has to go back to the network
    // more than once. The answer has to be identical and the only thing that moves is the count.
    let (source, refusals) = Reluctant::new(corpus(), 3);
    let mut decoder = decoder(ADDS_IT_UP, Box::new(source));

    let (stops, outcome) = drive(&mut decoder);
    outcome.expect("waiting longer for the same bytes does not change them");

    assert_eq!(refusals.load(Ordering::Relaxed), VALUES * 3);
    assert_eq!(stops as u64, VALUES * 3);
}

#[test]
fn a_source_that_never_misses_never_suspends() {
    let (source, refusals) = Reluctant::new(corpus(), 0);
    let mut decoder = decoder(ADDS_IT_UP, Box::new(source));

    let (stops, outcome) = drive(&mut decoder);
    outcome.expect("the same module, the same answer, with nothing to wait for");

    // The same decode against a source that has everything. A host reading a local file pays nothing
    // at all for a mechanism it is not using, and the decoder is not built differently to get that.
    assert_eq!(refusals.load(Ordering::Relaxed), 0);
    assert_eq!(stops, 0);
}

#[test]
fn a_range_past_the_end_is_an_answer_rather_than_the_end_of_the_scan() {
    let (source, _) = Reluctant::new(corpus(), 0);
    let mut decoder = decoder(ASKS_TOO_FAR, Box::new(source));

    let (_, outcome) = drive(&mut decoder);
    outcome.expect("a decoder told its range is out of bounds can ask for a smaller one");
}

#[test]
fn asking_with_no_source_attached_is_the_hosts_problem_and_says_so() {
    let program = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .compile(ASKS_ONCE.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("the module instantiates");

    // No `attach`, which is a host that ran a decoder that pulls without giving it anything to pull
    // from. The decoder is not at fault and the error does not blame it.
    let err = decoder
        .scan(&[])
        .wait()
        .expect_err("there is nothing to serve a range from");
    assert!(
        matches!(err, Error::NoSource),
        "a missing source should say so rather than {err}"
    );
}

#[test]
fn a_source_handed_over_can_be_taken_back() {
    let (source, _) = Reluctant::new(corpus(), 0);
    let mut decoder = decoder(ADDS_IT_UP, Box::new(source));

    let (_, outcome) = drive(&mut decoder);
    outcome.expect("the scan runs");

    // What a host does with it afterwards is read the counters on it, which is how M4 reports how
    // many requests a query made and how many bytes came back.
    let returned = decoder.detach().expect("the source is still there");
    assert_eq!(returned.len(), VALUES * 8);
    assert!(decoder.detach().is_none(), "it only comes back once");
}
