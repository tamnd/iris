//! The most important fuzz target in the project.
//!
//! `iris-guard` decides whether the Arrow arrays a sandboxed decoder handed back
//! are structurally sound. Every other check in iris fails loudly. This one
//! fails silently, by accepting an array that is not valid and letting the rest
//! of the process read it, so it gets the fuzzing budget.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The guard is not implemented yet. Once it is, this feeds arbitrary
    // buffers, offsets and validity bitmaps at it and asserts that anything it
    // accepts can then be read without going out of bounds.
    let _ = data;
});
