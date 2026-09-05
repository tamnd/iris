//! Metering: a call that does not come back is stopped, and nobody had to ask for that.
//!
//! The modules here are written in WebAssembly text rather than built from the SDK, because what is
//! being tested is the host and not a decoder. A hand written loop is unambiguous about what it is
//! doing, and it compiles in microseconds, which matters for a test whose failure mode is waiting.
//!
//! The end to end version of this, a real decoder built from source and run through a container,
//! lives in the `iris-runtime` gate test. This one is here so that a failure points at the crate
//! that owns the deadline.

use std::time::{Duration, Instant};

use iris_vm::{Decoder, Error, Vm};

/// Long enough that the epoch counter moves several times, short enough that a failing test is over
/// quickly. The counter ticks every ten milliseconds, so this is five ticks.
const DEADLINE: Duration = Duration::from_millis(50);

/// If a deadline of fifty milliseconds has not fired by now, it is not going to.
const PATIENCE: Duration = Duration::from_secs(5);

/// What the host calls the module under test. In iris this is a digest, and the only thing this
/// crate does with it is put it in the message.
const IDENTITY: &str = "blake3:4e2f1c";

/// A decoder that answers everything immediately.
const HONEST: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))
  (func (export "iris_scan") (result i64) (i64.const 0)))
"#;

/// The same decoder, except that its scan never returns.
const LOOPS_ON_SCAN: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))
  (func (export "iris_scan") (result i64)
    (loop $spin (br $spin))
    (i64.const 0)))
"#;

/// A module that never finishes being instantiated, because its start function loops.
///
/// This is the one a host gets wrong. Arming the deadline after instantiating looks correct and
/// passes every test that only calls exports, because a start function is guest code that runs
/// before any export is called.
const LOOPS_ON_START: &str = r#"
(module
  (memory (export "memory") 1)
  (start $spin)
  (func $spin (loop $l (br $l)))
  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))
  (func (export "iris_scan") (result i64) (i64.const 0)))
"#;

/// A vm with a deadline short enough to wait for.
fn vm() -> Vm {
    Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .with_deadline(DEADLINE)
}

#[test]
fn a_decoder_that_loops_forever_is_stopped() {
    let program = vm()
        .compile(LOOPS_ON_SCAN.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("it instantiates fine, it is the scan");

    let started = Instant::now();
    let err = decoder
        .scan(&[])
        .wait()
        .expect_err("the scan does not return");
    let waited = started.elapsed();

    assert!(
        matches!(err, Error::Deadline { .. }),
        "a loop should be a deadline rather than {err}"
    );
    assert!(
        waited < PATIENCE,
        "the deadline was {DEADLINE:?} and the call took {waited:?}"
    );
}

#[test]
fn the_message_names_the_decoder_and_the_budget() {
    let program = vm()
        .compile(LOOPS_ON_SCAN.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("it instantiates fine, it is the scan");

    let err = decoder
        .scan(&[])
        .wait()
        .expect_err("the scan does not return");
    let Error::Deadline { decoder, limit } = &err else {
        panic!("a loop should be a deadline rather than {err}");
    };

    assert_eq!(decoder, IDENTITY);
    assert_eq!(*limit, DEADLINE);
    assert!(
        err.to_string().contains(IDENTITY),
        "whoever reads this has to find the bytes that did it: {err}"
    );
}

#[test]
fn a_start_function_that_loops_is_stopped_too() {
    let program = vm()
        .compile(LOOPS_ON_START.as_bytes(), IDENTITY)
        .expect("the text compiles");

    let started = Instant::now();
    let err = Decoder::instantiate(&program).expect_err("instantiating runs the start function");
    let waited = started.elapsed();

    assert!(
        matches!(err, Error::Deadline { .. }),
        "a start function that loops should be a deadline rather than {err}"
    );
    assert!(
        waited < PATIENCE,
        "the deadline was {DEADLINE:?} and instantiating took {waited:?}"
    );
}

#[test]
fn every_call_gets_its_own_budget() {
    let program = vm()
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("the honest module instantiates");

    // The budget is per call, so a decoder that keeps answering keeps being answered. Spending
    // longer in total than one deadline is not the thing being metered, and a host that got this
    // wrong would fail somewhere in here rather than at the first call.
    for _ in 0..20 {
        decoder
            .load_source(b"")
            .expect("an honest decoder is not on a total budget");
        std::thread::sleep(DEADLINE / 4);
    }
}

#[test]
fn metering_is_on_without_anyone_configuring_it() {
    // No `with_deadline` anywhere here. The default budget is ten seconds, which is why this test
    // waits rather than asserting on a duration, and the point of it is that the default is finite.
    let program = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .compile(LOOPS_ON_SCAN.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let mut decoder = Decoder::instantiate(&program).expect("it instantiates fine, it is the scan");

    let err = decoder
        .scan(&[])
        .wait()
        .expect_err("the scan does not return");
    assert!(
        matches!(err, Error::Deadline { .. }),
        "the default should meter rather than {err}"
    );
}
