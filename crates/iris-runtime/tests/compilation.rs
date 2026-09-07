//! A decoder compiled by one runtime and reused by the next.
//!
//! `sharing.rs` is about the pool, which holds compiled decoders for as long as a process lives.
//! This is about the other half: a directory that holds them for longer than that, so a host that
//! comes up, answers one query and goes away does not pay the compiler every time.
//!
//! A second [`Runtime`] over the same directory is what a test has instead of a second process. It is
//! a fresh engine with an empty pool, which is the state a restart leaves, and the counters are what
//! say which path it took. None of these time anything, because a cache that quietly did nothing
//! would still give every caller the right rows at the speed it always cost.

mod support;

use iris_runtime::Runtime;

use support::{builder, column_values, flat_builder};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// [`ROWS`] as a length, because a column's values come back as a slice.
fn expected_rows() -> usize {
    usize::try_from(ROWS).expect("a few hundred rows fit in a usize on anything this runs on")
}

#[test]
fn a_second_runtime_over_the_same_directory_does_not_compile_again() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bytes = builder(ROWS, 2).build().expect("the container is writable");

    let cold = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    cold.open(&bytes).expect("the container opens");
    assert_eq!(cold.compilations_stored(), 1);
    assert_eq!(cold.compilations_reused(), 0);

    let warm = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    let dataset = warm.open(&bytes).expect("the container opens");
    let batches = dataset.scan().expect("the scan runs");

    assert_eq!(
        warm.compilations_reused(),
        1,
        "the second runtime should have found what the first one left"
    );
    assert_eq!(warm.compilations_stored(), 0);
    assert_eq!(
        column_values(&batches, 0).len(),
        expected_rows(),
        "and a decoder loaded off disk reads the whole table"
    );
}

#[test]
fn the_pool_is_asked_before_the_directory_is() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bytes = builder(ROWS, 2).build().expect("the container is writable");

    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    for _ in 0..4 {
        runtime.open(&bytes).expect("the container opens");
    }

    assert_eq!(
        runtime.decoders_compiled(),
        1,
        "a module already compiled in this process costs nothing and is not looked up"
    );
    assert_eq!(runtime.compilations_stored(), 1);
    assert_eq!(runtime.compilations_reused(), 0);
}

#[test]
fn a_host_that_keeps_nothing_in_memory_still_keeps_it_on_disk() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bytes = builder(ROWS, 2).build().expect("the container is writable");

    // No pool at all, which is the arrangement that shows the two layers are separate. Every open
    // misses in memory, and every open after the first finds the artefact.
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_decoder_cache_bytes(0)
        .with_compilation_cache(dir.path());
    for _ in 0..3 {
        runtime.open(&bytes).expect("the container opens");
    }

    assert_eq!(runtime.decoders_compiled(), 3, "nothing was held in memory");
    assert_eq!(runtime.compilations_stored(), 1);
    assert_eq!(runtime.compilations_reused(), 2);
}

#[test]
fn two_decoders_are_two_entries_and_neither_finds_the_other() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let fixed = builder(ROWS, 2).build().expect("the container is writable");
    let flat = flat_builder(ROWS)
        .build()
        .expect("the container is writable");

    let cold = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    for container in [&fixed, &flat] {
        cold.open(container).expect("the container opens");
    }
    assert_eq!(cold.compilations_stored(), 2);

    let warm = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    for container in [&fixed, &flat] {
        let dataset = warm.open(container).expect("the container opens");
        assert_eq!(dataset.rows(), ROWS);
    }
    assert_eq!(warm.compilations_reused(), 2);
    assert_eq!(warm.compilations_stored(), 0);
}

#[test]
fn nothing_is_kept_when_no_directory_was_named() {
    let bytes = builder(ROWS, 2).build().expect("the container is writable");
    let runtime = Runtime::new().expect("a runtime starts");
    runtime.open(&bytes).expect("the container opens");

    assert_eq!(runtime.compilations_stored(), 0);
    assert_eq!(runtime.compilations_reused(), 0);
}

#[test]
fn a_directory_that_cannot_be_written_is_not_an_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let blocked = dir.path().join("occupied");
    std::fs::write(&blocked, b"in the way").expect("the temporary directory is writable");
    let bytes = builder(ROWS, 2).build().expect("the container is writable");

    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(&blocked)
        .with_decoder_cache_bytes(0);
    for _ in 0..2 {
        let dataset = runtime
            .open(&bytes)
            .expect("a cache that cannot work must not fail an open");
        let batches = dataset.scan().expect("the scan runs");
        assert_eq!(column_values(&batches, 0).len(), expected_rows());
    }

    assert_eq!(runtime.compilations_stored(), 0);
    assert_eq!(runtime.compilations_reused(), 0);
}

#[test]
fn a_clone_shares_the_directory_and_the_tally() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let bytes = builder(ROWS, 2).build().expect("the container is writable");

    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_compilation_cache(dir.path());
    let handed_over = runtime.clone();
    handed_over.open(&bytes).expect("the container opens");

    assert_eq!(
        runtime.compilations_stored(),
        1,
        "a clone is how a runtime reaches the threads that use it, and the tally has to survive that"
    );
}
