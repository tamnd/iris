//! The compilation cache: what it reuses, what it refuses, and what it does when it cannot work.
//!
//! The modules here are WebAssembly text for the same reason the deadline tests are. What is being
//! checked is which of two paths the host took, not what a decoder computed, and a hand written
//! module makes it obvious that two of these differ and two of them do not.
//!
//! A cache is hard to test by its effect, because the effect is that something was faster. So none of
//! these time anything. They check the counters the cache keeps, the files it leaves behind, and that
//! a decoder loaded out of the directory is one that still runs.

use std::fs;
use std::path::Path;

use iris_vm::{Decoder, Vm};

/// A decoder that answers everything immediately, which is all these tests need of one.
const HONEST: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))
  (func (export "iris_scan") (result i64) (i64.const 0)))
"#;

/// The same decoder with one more function, so that it is a different module and a different key.
const ALSO_HONEST: &str = r#"
(module
  (memory (export "memory") 1)
  (func $unused (result i32) (i32.const 7))
  (func (export "iris_source") (param i32) (result i32) (i32.const 0))
  (func (export "iris_input") (param i32) (result i32) (i32.const 0))
  (func (export "iris_start") (result i64) (i64.const 0))
  (func (export "iris_scan") (result i64) (i64.const 0)))
"#;

/// What the host calls the module under test. In iris this is a digest.
const IDENTITY: &str = "blake3:4e2f1c";

/// A fresh engine pointed at this directory.
///
/// Fresh rather than cloned, because these are about what survives a process and the nearest thing a
/// test has to a second process is a second engine that has compiled nothing.
fn vm(dir: &Path) -> Vm {
    Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .with_compilation_cache(dir)
}

/// The artefacts in a directory, ignoring anything else that ended up there.
fn artefacts(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .map(|entry| entry.expect("the directory is readable").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".iris-aot"))
        .collect();
    found.sort();
    found
}

#[test]
fn the_first_compile_stores_and_the_second_engine_reuses() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    let cold = vm(dir.path());
    cold.compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    assert_eq!(cold.compilations_stored(), 1);
    assert_eq!(cold.compilations_reused(), 0);
    assert_eq!(artefacts(dir.path()).len(), 1);

    let warm = vm(dir.path());
    warm.compile(HONEST.as_bytes(), IDENTITY)
        .expect("what the cold engine left is loadable");
    assert_eq!(warm.compilations_reused(), 1);
    assert_eq!(
        warm.compilations_stored(),
        0,
        "a hit should not write the entry it just read"
    );
    assert_eq!(
        artefacts(dir.path()).len(),
        1,
        "the same decoder is one entry however many engines open it"
    );
}

#[test]
fn a_decoder_out_of_the_directory_still_runs() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    vm(dir.path())
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");

    let warm = vm(dir.path());
    let program = warm
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("what the cold engine left is loadable");
    assert_eq!(warm.compilations_reused(), 1, "this has to be the hit path");

    // The point of the whole thing. A cache that hands back something that will not instantiate is
    // worse than no cache, so the reused module goes all the way through a handshake.
    let mut decoder = Decoder::instantiate(&program).expect("the loaded module instantiates");
    decoder
        .load_source(b"")
        .expect("and it answers a call like any other");
    assert_eq!(program.decoder(), IDENTITY, "the name is still the host's");
}

#[test]
fn two_decoders_get_two_entries() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    let engine = vm(dir.path());
    engine
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    engine
        .compile(ALSO_HONEST.as_bytes(), IDENTITY)
        .expect("and so does the other one");

    assert_eq!(engine.compilations_stored(), 2);
    assert_eq!(
        engine.compilations_reused(),
        0,
        "different bytes must not find each other's entry"
    );
    assert_eq!(artefacts(dir.path()).len(), 2);
}

#[test]
fn the_same_engine_reuses_what_it_wrote_itself() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    let engine = vm(dir.path());
    engine
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    engine
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("and again");

    assert_eq!(engine.compilations_stored(), 1);
    assert_eq!(engine.compilations_reused(), 1);
}

#[test]
fn a_clone_shares_the_directory_and_the_tally() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    let engine = vm(dir.path());
    let handed_over = engine.clone();
    handed_over
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");

    assert_eq!(
        engine.compilations_stored(),
        1,
        "a clone that kept its own tally would be a tally nobody can read"
    );
    assert_eq!(handed_over.compilations_stored(), 1);
}

#[test]
fn rubbish_in_the_directory_is_compiled_around() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    vm(dir.path())
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let entry = artefacts(dir.path())
        .pop()
        .expect("the cold run left an entry");
    fs::write(dir.path().join(&entry), b"not machine code, not anything")
        .expect("the entry is writable");

    // Wasmtime refuses it, and the only thing a host notices is that it compiled.
    let warm = vm(dir.path());
    warm.compile(HONEST.as_bytes(), IDENTITY)
        .expect("a bad entry is not an error");
    assert_eq!(warm.compilations_reused(), 0);
    assert_eq!(
        warm.compilations_stored(),
        1,
        "and the bad entry gets replaced rather than tripped over again"
    );
}

#[test]
fn a_truncated_entry_is_compiled_around_too() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    vm(dir.path())
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");
    let entry = artefacts(dir.path())
        .pop()
        .expect("the cold run left an entry");
    let path = dir.path().join(&entry);
    let whole = fs::read(&path).expect("the entry is readable");
    fs::write(&path, &whole[..whole.len() / 2]).expect("the entry is writable");

    // This is the shape an interrupted write would leave if writes were not atomic, and it is the
    // one worth checking separately from rubbish: the header is genuine and the rest is missing.
    let warm = vm(dir.path());
    warm.compile(HONEST.as_bytes(), IDENTITY)
        .expect("half an entry is not an error either");
    assert_eq!(warm.compilations_reused(), 0);
}

#[test]
fn a_directory_that_cannot_be_written_is_not_an_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // A file where the directory should be, which nothing can create a directory at or write into.
    let blocked = dir.path().join("occupied");
    fs::write(&blocked, b"in the way").expect("the temporary directory is writable");

    let engine = vm(&blocked);
    engine
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("a cache that cannot work must not fail an open");
    assert_eq!(engine.compilations_stored(), 0);
    assert_eq!(engine.compilations_reused(), 0);
}

#[test]
fn two_engines_built_the_same_way_have_the_same_fingerprint() {
    // If this were not so, nothing would ever be reused, because the fingerprint is half the key.
    // It is the property that makes a cache possible rather than a property of the cache.
    let one = Vm::new().expect("an engine builds and the epoch thread starts");
    let two = Vm::new().expect("an engine builds and the epoch thread starts");
    assert_eq!(one.fingerprint(), two.fingerprint());
}

#[test]
fn the_deadline_is_not_part_of_the_fingerprint() {
    // A deadline is the host's patience and not a compiler setting, so two hosts that disagree about
    // it compile the same machine code and should share entries. The check is here rather than left
    // implicit because the deadline lives on the same type as the fingerprint and it would be an easy
    // thing to fold in by accident.
    let patient = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .with_deadline(std::time::Duration::from_secs(300));
    let hurried = Vm::new()
        .expect("an engine builds and the epoch thread starts")
        .with_deadline(std::time::Duration::from_millis(50));
    assert_eq!(patient.fingerprint(), hurried.fingerprint());
}

#[test]
fn a_host_that_is_impatient_finds_what_a_patient_one_left() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    vm(dir.path())
        .with_deadline(std::time::Duration::from_secs(300))
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");

    let hurried = vm(dir.path()).with_deadline(std::time::Duration::from_millis(50));
    let program = hurried
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("what the patient engine left is loadable");

    assert_eq!(hurried.compilations_reused(), 1);
    assert_eq!(
        program.deadline(),
        std::time::Duration::from_millis(50),
        "and the module carries this host's budget rather than the one that compiled it"
    );
}

#[test]
fn nothing_is_written_when_no_directory_was_named() {
    let engine = Vm::new().expect("an engine builds and the epoch thread starts");
    engine
        .compile(HONEST.as_bytes(), IDENTITY)
        .expect("the text compiles");

    assert_eq!(engine.compilations_stored(), 0);
    assert_eq!(engine.compilations_reused(), 0);
}
