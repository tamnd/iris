//! One runtime, many threads, and the decoder compiled once.
//!
//! The pool inside a [`Runtime`] is checked from two directions. The loom models in the crate
//! enumerate the interleavings of the handoff and the eviction, with an integer standing in for a
//! compiled module because a model runs thousands of times. These are the other half: the same pool
//! with a real decoder in it, driven by real threads, checking that what the models are about is
//! what is actually wired up.
//!
//! The number they assert on is how many times the compiler ran, and it has to be that rather than
//! anything about the rows. A pool that quietly does nothing gives every caller the right answer,
//! costs what it always cost, and would pass any test written about what came back.

mod support;

use std::sync::Arc;
use std::thread;

use iris_runtime::Runtime;

use support::{builder, column_values, flat_builder};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// Enough threads that they overlap on an ordinary machine, and few enough to be polite.
const THREADS: usize = 8;

#[test]
fn eight_threads_opening_one_container_compile_the_decoder_once() {
    let bytes = Arc::new(
        builder(ROWS, 2)
            .build()
            .expect("the container is writable"),
    );
    let runtime = Runtime::new().expect("a runtime starts");

    let readers: Vec<_> = (0..THREADS)
        .map(|_| {
            let runtime = runtime.clone();
            let bytes = Arc::clone(&bytes);
            thread::spawn(move || {
                let dataset = runtime.open(&bytes).expect("the container opens");
                let batches = dataset.scan().expect("the scan runs");
                column_values(&batches, 0)
            })
        })
        .collect();

    for reader in readers {
        let values = reader.join().expect("no reader panics");
        assert_eq!(
            values.len(),
            ROWS as usize,
            "a thread that shared a compiled decoder still read the whole table"
        );
    }

    assert_eq!(
        runtime.decoders_compiled(),
        1,
        "eight opens of one decoder are one compile, whichever thread got there first"
    );
    assert_eq!(runtime.decoders_cached(), 1);
}

#[test]
fn two_decoders_are_both_held_and_neither_is_compiled_twice() {
    let fixed = builder(ROWS, 2).build().expect("the container is writable");
    let flat = flat_builder(ROWS)
        .build()
        .expect("the container is writable");
    let runtime = Runtime::new().expect("a runtime starts");

    for _ in 0..3 {
        for bytes in [&fixed, &flat] {
            let dataset = runtime.open(bytes).expect("the container opens");
            assert_eq!(dataset.rows(), ROWS);
        }
    }

    assert_eq!(
        runtime.decoders_compiled(),
        2,
        "two decoders, three opens each, and one compile apiece"
    );
    assert_eq!(runtime.decoders_cached(), 2);
    assert!(runtime.decoders_cached_bytes() > 0);
}

#[test]
fn a_budget_of_nothing_is_how_a_host_says_it_does_not_want_this() {
    let bytes = builder(ROWS, 2).build().expect("the container is writable");
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_decoder_cache_bytes(0);

    for _ in 0..3 {
        runtime.open(&bytes).expect("the container opens");
    }

    assert_eq!(
        runtime.decoders_compiled(),
        3,
        "nothing fits in a budget of nothing, so every open compiles"
    );
    assert_eq!(runtime.decoders_cached(), 0);
    assert_eq!(runtime.decoders_cached_bytes(), 0);
}

#[test]
fn a_deadline_set_after_a_decoder_was_compiled_still_applies_to_it() {
    let bytes = builder(ROWS, 2).build().expect("the container is writable");
    let runtime = Runtime::new().expect("a runtime starts");
    runtime.open(&bytes).expect("the container opens");

    // The compiled module is shared and the deadline is not part of it, so this open takes the
    // module the line above compiled and stamps its own budget on it. A pool that keyed on the
    // deadline as well would compile a second copy here, and a pool that ignored the deadline
    // entirely would hand back a decoder metered against somebody else's patience.
    let patient = runtime
        .clone()
        .with_decoder_deadline(std::time::Duration::from_secs(30));
    let dataset = patient.open(&bytes).expect("the container opens");
    let batches = dataset.scan().expect("the scan runs");
    assert_eq!(column_values(&batches, 0).len(), ROWS as usize);

    assert_eq!(
        runtime.decoders_compiled(),
        1,
        "a clone with a different deadline shares the pool and the module in it"
    );
}
