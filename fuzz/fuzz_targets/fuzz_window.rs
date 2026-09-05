//! Slide a window around a file in an order nobody thought of, and check every byte and every
//! vacated address.
//!
//! This is the target `tamnd/iris` #20 asks for. The stress test in `crates/iris-source` walks a
//! fixed stride, which is the access pattern a scan has and the one worth being sure about, and it
//! runs on all four platforms on every change. What it cannot do is find the order of requests that
//! breaks the state machine, because it only ever performs one order. A request that lands one byte
//! before the current view, then one byte after it, then in the middle, then at the end of the file,
//! is the kind of sequence a fuzzer produces in a minute and a person writing a test does not think
//! of.
//!
//! The input is read as a list of requests rather than as bytes to map, because the interesting
//! space here is the sequence of offsets and not the contents of a file. The file is fixed, created
//! once per process, and filled with a pattern that is a function of each byte's own offset, so a
//! byte that came from the wrong place is wrong on its face without knowing where it came from. That
//! is what makes a stale read detectable at all: it returns real bytes from a real part of the file,
//! and the only thing separating it from a correct read is whether the bytes are at the offset they
//! claim to be.
//!
//! Two things are asserted. Every byte handed back matches the pattern for its offset, which catches
//! a view mapped at the wrong place or shorter than it claims. And every address inside the
//! reservation that the current view does not cover is unreadable, which catches the slide that left
//! the old mapping in place. The second one is asked of the operating system rather than by reading
//! the address, since reading it is the thing being asserted will not work.

#![no_main]

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use iris_source::probe::readable;
use iris_source::{Window, WindowError};
use libfuzzer_sys::fuzz_target;

/// How long the sample file is. Large enough that a small span cannot cover it and every request has
/// somewhere to slide to, small enough that creating it once costs nothing.
const FILE_LEN: u64 = 4 << 20;

/// The byte the sample file holds at `offset`.
///
/// splitmix64's finaliser, so every output bit depends on every input bit and two offsets a power of
/// two apart produce unrelated bytes. That matters here for the same reason it matters in the
/// integration test: every view starts on an alignment boundary, so a pattern with a period that is
/// a power of two makes the start of one view identical to the start of every other one, and a stale
/// read stops being detectable.
///
/// The same function the integration test uses, deliberately duplicated rather than shared. The fuzz
/// package is outside the workspace and depends on iris-source as a published surface, and exporting
/// a test pattern from a library crate to avoid six lines here would be the wrong trade.
fn pattern(offset: u64) -> u8 {
    let mut mixed = offset.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((mixed ^ (mixed >> 31)) & 0xff) as u8
}

/// The sample file, created on first use and kept for the life of the process.
///
/// The directory is leaked on purpose. A fuzz target has no place to run teardown, and a temporary
/// directory dropped at the end of the first input would take the file out from under every input
/// after it.
fn sample() -> &'static PathBuf {
    static SAMPLE: OnceLock<PathBuf> = OnceLock::new();
    SAMPLE.get_or_init(|| {
        let dir = Box::leak(Box::new(
            tempfile::tempdir().expect("a temporary directory"),
        ));
        let path = dir.path().join("sample.bin");
        let mut file = File::create(&path).expect("creating the sample");
        let mut block = vec![0u8; 64 * 1024];
        let mut written = 0u64;
        while written < FILE_LEN {
            for (i, slot) in block.iter_mut().enumerate() {
                *slot = pattern(written + i as u64);
            }
            file.write_all(&block).expect("writing the sample");
            written += block.len() as u64;
        }
        file.sync_all().expect("flushing the sample");
        path
    })
}

/// One request: where to read from and how much.
struct Request {
    at: u64,
    len: usize,
}

/// Reads the input as a span to reserve and a list of requests.
///
/// Six bytes a request, which is four for an offset and two for a length. Both are taken modulo
/// something that keeps them mostly in range rather than clamped into it, because a request past the
/// end of the file is a case worth reaching and an input that only ever produces valid requests never
/// exercises the refusal path.
fn plan(data: &[u8]) -> Option<(usize, Vec<Request>)> {
    let (first, rest) = data.split_first()?;

    // Four spans, all small enough that the file cannot fit in one, so nearly every request has to
    // slide. A span large enough to hold the file would turn this into a test of one mapping.
    let span = match first & 0b11 {
        0 => 64 * 1024,
        1 => 128 * 1024,
        2 => 192 * 1024,
        _ => 320 * 1024,
    };

    let requests = rest
        .chunks_exact(6)
        .map(|chunk| {
            let at = u64::from(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            let len = usize::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            Request {
                // Past the end of the file about a quarter of the time, which is the bounds check.
                at: at % (FILE_LEN + FILE_LEN / 4),
                len,
            }
        })
        .take(2048)
        .collect();

    Some((span, requests))
}

fuzz_target!(|data: &[u8]| {
    let Some((span, requests)) = plan(data) else {
        return;
    };

    let file = File::open(sample()).expect("opening the sample");
    let mut window = Window::with_span(file, span).expect("opening a window");
    let base = window.address();
    let reserved = window.span();

    for request in requests {
        let bytes = match window.range(request.at, request.len) {
            Ok(bytes) => bytes,
            // Both refusals are expected outcomes for a request the input chose freely, and both are
            // checked rather than swallowed: out of bounds only for a range that really does run past
            // the end, too large only for one that really does not fit in a view.
            Err(WindowError::OutOfBounds { .. }) => {
                assert!(
                    request.at.saturating_add(request.len as u64) > FILE_LEN,
                    "a range inside the file was refused as out of bounds"
                );
                continue;
            }
            Err(WindowError::TooLarge { wanted, .. }) => {
                assert!(wanted > span, "a range that fits was refused as too large");
                continue;
            }
            Err(other) => panic!("the operating system refused a slide: {other}"),
        };

        assert_eq!(bytes.len(), request.len);
        for (i, got) in bytes.iter().enumerate() {
            let offset = request.at + i as u64;
            assert_eq!(
                *got,
                pattern(offset),
                "offset {offset} came back as {got}, so it was read from somewhere else"
            );
        }

        // The reservation never moves and never changes size, however the view moves inside it.
        assert_eq!(window.address(), base, "the reservation moved");
        assert_eq!(window.span(), reserved, "the reservation changed size");

        // Everything past the current view is either a placeholder or PROT_NONE, and either way a
        // decoder that kept an address from an earlier view must not be able to read through it.
        if let Some((_, mapped)) = window.mapped()
            && mapped < reserved
        {
            // SAFETY: forming an address inside the reservation, which is `reserved` bytes long and
            // owned by the window for as long as it lives. Nothing dereferences it, `readable` asks
            // the kernel about the address rather than reading it.
            let vacated = unsafe { base.add(mapped) };
            assert!(
                !readable(vacated, 1),
                "an address outside the current view is still readable, so a slide left the old \
                 mapping behind and a decoder holding that address would read stale bytes"
            );
        }
    }
});
