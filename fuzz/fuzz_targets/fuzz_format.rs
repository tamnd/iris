//! Container parsing against arbitrary bytes.
//!
//! A dataset is an untrusted input. Parsing one must not panic, must not read
//! out of bounds, and must not allocate on the basis of a length field it has
//! not validated.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The parser is not implemented yet.
    let _ = data;
});
