//! Container parsing against arbitrary bytes.
//!
//! A dataset is an untrusted input. Parsing one must not panic, must not read
//! out of bounds, and must not allocate on the basis of a length field it has
//! not validated.
//!
//! The digest check is skipped here on purpose. With it on, essentially every
//! input the fuzzer generates is rejected at the trailer and the parser behind
//! it never runs, so the target would spend its whole budget proving that
//! BLAKE3 works.

#![no_main]

use iris_format::Container;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(container) = Container::parse_without_root_digest(data) else {
        return;
    };

    // Anything the parser accepted has to be readable without going out of
    // bounds, so read all of it.
    let _ = container.header();
    let _ = container.dataset();
    let _ = container.schema();
    let _ = container.decoder();
    let _ = container.decoder_bytes();
    for section in container.sections() {
        let bytes = container.section_bytes(section);
        assert_eq!(
            bytes.len() as u64,
            section.len,
            "a section that parsed did not hand back the bytes it claimed"
        );
    }
    let _ = container.verify();
});
