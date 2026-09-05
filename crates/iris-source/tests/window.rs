//! The gate for the sliding window, on every platform that has one.
//!
//! Two milestone issues live here. `tamnd/iris` #19 asks for thousands of slide and remap cycles
//! with no address space leak, no stale view and no handle leak, running in CI on every change.
//! `tamnd/iris` #20 asks that a pointer held across a slide traps or reads zeroes and never returns
//! data from the range that used to be there.
//!
//! Both are about Windows more than anything else. Unix moves a view with one `mmap` and the
//! addresses are never let go of. Windows needs four calls in a fixed order with three different
//! placeholder flags, and getting the order wrong does not fail: it leaks the reservation, or leaves
//! a split placeholder that the next map trips over a thousand cycles later. That is a class of bug
//! a unit test does not find and a stress test does, which is why the gate is written this way.
//!
//! These tests are deliberately arithmetic rather than fixture based. The file is filled with a
//! pattern that is a function of the byte's own offset, so any byte read from the wrong place is
//! detectable from its value alone, without knowing where it came from. A stale view returns real
//! bytes from a real part of the file, which is exactly what makes it dangerous, and the only thing
//! that separates those from correct bytes is whether they are at the offset they claim to be.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the file pattern is a byte by construction"
)]

use std::fs::File;
use std::io::Write as _;

use iris_source::{Window, WindowError};

/// How many handles the process holds, when this build can find out.
///
/// The probe is behind a feature, so without it the handle check in the stress test below skips
/// rather than failing to compile. CI runs these with every feature on, which is the configuration
/// the milestone gate is about.
#[cfg(feature = "probe")]
fn handles() -> Option<u32> {
    iris_source::probe::handles()
}

#[cfg(not(feature = "probe"))]
fn handles() -> Option<u32> {
    None
}

/// The byte this file holds at `offset`.
///
/// This is splitmix64's finaliser, which is an avalanche mix: every output bit depends on every
/// input bit, so two offsets a power of two apart produce unrelated bytes.
///
/// The first version of this was two multipliers and a shift, and it repeated every sixty four
/// kibibytes, because the shifted term only kept bits that depend on the offset modulo two to the
/// sixteen. Every view starts on a multiple of the allocation granularity, which on Windows is
/// exactly sixty four kibibytes, so the bytes at the start of one view were identical to the bytes
/// at the start of every other view. The assertion that the front of the reservation no longer holds
/// the bytes from before a slide could not distinguish a correct remap from no remap at all, and
/// Windows is where it was caught, because that is the platform whose granularity lines up with the
/// period. A test pattern needs a mixer, not an arithmetic sequence.
fn pattern(offset: u64) -> u8 {
    let mut mixed = offset.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((mixed ^ (mixed >> 31)) & 0xff) as u8
}

/// A temporary file of `len` bytes filled with [`pattern`], and the handle to read it back.
fn sample(len: u64) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("sample.bin");
    let mut file = File::create(&path).expect("creating the sample");
    let mut written = 0u64;
    let mut block = vec![0u8; 64 * 1024];
    while written < len {
        let this = block.len().min((len - written) as usize);
        for (i, slot) in block[..this].iter_mut().enumerate() {
            *slot = pattern(written + i as u64);
        }
        file.write_all(&block[..this]).expect("writing the sample");
        written += this as u64;
    }
    file.sync_all().expect("flushing the sample");
    (dir, path)
}

/// Every byte of `bytes` is what the file holds at `at` onwards.
fn assert_matches(bytes: &[u8], at: u64) {
    for (i, got) in bytes.iter().enumerate() {
        let offset = at + i as u64;
        assert_eq!(
            *got,
            pattern(offset),
            "byte at offset {offset} came back as {got} and the file holds {}",
            pattern(offset)
        );
    }
}

#[test]
fn a_window_reads_the_bytes_that_are_actually_there() {
    let (_dir, path) = sample(1 << 20);
    let file = File::open(&path).expect("opening the sample");
    let mut window = Window::with_span(file, 128 * 1024).expect("opening a window");

    assert_eq!(window.len(), 1 << 20);
    assert!(!window.is_empty());

    for at in [
        0u64,
        1,
        4095,
        4096,
        65_535,
        65_536,
        100_000,
        (1 << 20) - 1024,
    ] {
        let len = usize::try_from((1u64 << 20) - at).unwrap().min(1024);
        let bytes = window.range(at, len).expect("a range inside the file");
        assert_eq!(bytes.len(), len);
        assert_matches(bytes, at);
    }
}

#[test]
fn a_range_inside_the_current_view_does_not_slide() {
    let (_dir, path) = sample(1 << 20);
    let file = File::open(&path).expect("opening the sample");
    let mut window = Window::with_span(file, 512 * 1024).expect("opening a window");

    window.range(0, 16).expect("the first range");
    let after_first = window.slides();
    assert_eq!(after_first, 1, "the first range has to map something");

    // Everything here is inside the first view, so none of it may remap. This is the property the
    // whole structure exists for: if a clustered read still slid on every request, a window would be
    // strictly worse than mapping the file.
    for at in (0..4096).step_by(64) {
        window.range(at, 64).expect("a range inside the first view");
    }
    assert_eq!(
        window.slides(),
        after_first,
        "a range inside the view must not remap"
    );
}

#[test]
fn the_last_view_of_a_file_covers_the_tail() {
    // A length that is not a multiple of any alignment on any platform, so the final view has to be
    // rounded up past the end of the file and the bytes past it are never handed out.
    let len = 300_001u64;
    let (_dir, path) = sample(len);
    let file = File::open(&path).expect("opening the sample");
    let mut window = Window::with_span(file, 64 * 1024).expect("opening a window");

    let bytes = window.range(len - 1, 1).expect("the last byte");
    assert_eq!(bytes[0], pattern(len - 1));

    let bytes = window
        .range(len - 17, 17)
        .expect("the last seventeen bytes");
    assert_matches(bytes, len - 17);

    // One past the end is an error rather than a zero, because a view is rounded up to a page and a
    // read of the padding would otherwise look like a successful read of nothing.
    let refused = window.range(len, 1).unwrap_err();
    assert!(
        matches!(refused, WindowError::OutOfBounds { .. }),
        "got {refused:?}"
    );
    let refused = window.range(len - 1, 2).unwrap_err();
    assert!(
        matches!(refused, WindowError::OutOfBounds { .. }),
        "got {refused:?}"
    );
}

#[test]
fn an_empty_file_opens_and_serves_nothing() {
    let (_dir, path) = sample(0);
    let file = File::open(&path).expect("opening the sample");
    let mut window = Window::with_span(file, 64 * 1024).expect("opening a window");

    assert!(window.is_empty());
    assert_eq!(window.range(0, 0).expect("a zero length read"), b"");
    assert!(matches!(
        window.range(0, 1).unwrap_err(),
        WindowError::OutOfBounds { .. }
    ));
    assert_eq!(window.slides(), 0, "there is nothing to map");
}

/// A zero length read at exactly the end of the file, which the fuzzer found in its first minute.
///
/// The offset is in bounds, because a range of nothing that starts at the end ends at the end. The
/// view that would cover it is empty, because there is nothing after the end of the file, and a
/// mapping of zero bytes is refused by both platforms with `EINVAL` and nothing useful in it. So the
/// window has to recognise that there is nothing to map rather than ask for it, which is the same
/// state it is already in for an empty file.
///
/// This is not a contrived input. It is what a decoder produces when it asks for a column that
/// happens to be empty and happens to sit last in the file.
#[test]
fn a_zero_length_read_at_the_end_of_the_file_maps_nothing() {
    let len = 300_001u64;
    let (_dir, path) = sample(len);
    let file = File::open(&path).expect("opening the sample");
    let mut window = Window::with_span(file, 64 * 1024).expect("opening a window");

    assert_eq!(
        window.range(len, 0).expect("a zero length read at the end"),
        b""
    );

    // Both orders, because the interesting one is the second: reaching the end with a view already
    // mapped means the empty read has to take the old view down and put nothing back.
    let bytes = window.range(len - 8, 8).expect("the last eight bytes");
    assert_matches(bytes, len - 8);
    assert_eq!(
        window
            .range(len, 0)
            .expect("a zero length read after a view exists"),
        b""
    );

    // And the window still works afterwards, which is what says it was left in a state it can slide
    // out of rather than one where every further request fails.
    let bytes = window.range(0, 64).expect("a read after the empty read");
    assert_matches(bytes, 0);
}

#[test]
fn a_request_larger_than_the_span_is_refused_rather_than_half_served() {
    let (_dir, path) = sample(1 << 20);
    let file = File::open(&path).expect("opening the sample");
    let span = 128 * 1024;
    let mut window = Window::with_span(file, span).expect("opening a window");

    let refused = window.range(0, span + 1).unwrap_err();
    assert!(
        matches!(refused, WindowError::TooLarge { .. }),
        "got {refused:?}"
    );

    // Exactly the span, but starting part of the way into an alignment unit, needs more than the
    // span in one view because a view cannot start anywhere else. This is the case that would have
    // silently come up short if the check only looked at the requested length.
    let refused = window.range(1, span).unwrap_err();
    assert!(
        matches!(refused, WindowError::TooLarge { .. }),
        "got {refused:?}"
    );

    // Aligned and exactly the span is fine.
    let bytes = window.range(0, span).expect("a span sized aligned range");
    assert_matches(&bytes[..512], 0);
}

/// The stress test issue #19 asks for.
///
/// Thousands of cycles, each one a slide to a place the current view does not cover, each one
/// verified by reading bytes whose value proves where they came from. What this is looking for is
/// not a wrong answer on cycle one, which any of the tests above would catch. It is the failure that
/// only appears after the state machine has been round the loop enough times: a placeholder that was
/// split and never coalesced, a view that was mapped over a range that was never unmapped, a section
/// handle created per slide instead of once.
///
/// Three things are asserted at the end, and they are the three failure modes in the issue.
///
/// The address does not move, which is the whole reason for reserving rather than mapping. If the
/// reservation were being released and retaken, this would drift, and on Windows that is exactly
/// what happens if `MEM_PRESERVE_PLACEHOLDER` is left off one of the two calls that need it.
///
/// The reservation is still the size it started as, and every read still lands, so the address space
/// is not leaking in pieces.
///
/// No handles accumulate. Windows creates a section object per backing, and a section created per
/// slide rather than per window is four thousand handles by the end of this loop, with every read
/// still correct and nothing else to notice it by.
#[test]
fn thousands_of_slides_leak_nothing_and_never_go_stale() {
    let len = 8 << 20;
    let (_dir, path) = sample(len);
    let file = File::open(&path).expect("opening the sample");
    // Two allocation units on Windows and many more everywhere else, which is the smallest span that
    // can serve a read wherever it lands. A view starts on an allocation boundary, so a span of
    // exactly one unit cannot cover a request that straddles one, and on Windows a unit is sixty
    // four kibibytes. A span of one unit passed everywhere else and refused a read two thousand
    // cycles in on Windows, which is the shape of a bug this test is otherwise looking for and is
    // instead the span being too small to ask the question.
    let span = 128 * 1024;
    let mut window = Window::with_span(file, span).expect("opening a window");

    let base = window.address();
    let reserved = window.span();
    // Taken after the window is open, so the file and the section it needs are already counted and
    // the difference at the end is only what the loop added.
    let handles_before = handles();

    // Longer than the span, so every cycle lands outside the current view and has to slide, and odd,
    // so it is coprime with every alignment on every platform and the walk does not settle into a
    // short cycle that only exercises two views. Walking forwards then wrapping is also the access
    // pattern a scan has, which is the one worth being sure about.
    let stride = 163_841u64;
    let cycles = 4000u64;
    let read = 512usize;

    let mut at = 0u64;
    for cycle in 0..cycles {
        let bytes = window.range(at, read).unwrap_or_else(|e| {
            panic!("cycle {cycle} at offset {at} failed: {e}");
        });
        assert_matches(bytes, at);

        // A second read at the far end of the same view, which catches a view that was mapped
        // shorter than it claims. That failure reads correctly at the start and faults or returns
        // padding at the end, and only the second read sees it.
        if let Some((view_at, view_len)) = window.mapped() {
            let last = (view_at + view_len as u64 - 1).min(len - 1);
            let bytes = window
                .range(last, 1)
                .expect("the last byte of the current view");
            assert_eq!(
                bytes[0],
                pattern(last),
                "the far end of the view at cycle {cycle}"
            );
        }

        at = (at + stride) % (len - read as u64);
    }

    assert!(
        window.slides() > cycles / 2,
        "the stride has to actually move the view"
    );
    assert_eq!(
        window.address(),
        base,
        "the reservation moved, so it was released and retaken"
    );
    assert_eq!(window.span(), reserved, "the reservation changed size");

    // A handle leak is a count, so it is checked as one. The tolerance is not zero because a test
    // harness is free to open something of its own while this runs, and it does not need to be
    // tight: the failure being looked for is one handle per cycle, which is four thousand of them.
    if let (Some(before), Some(after)) = (handles_before, handles()) {
        assert!(
            after <= before + 8,
            "the process held {before} handles before {cycles} slides and {after} after, so a slide \
             is keeping one"
        );
    }

    // And the window still works after all of it, which rules out a state machine that has wedged
    // into a state where every further slide fails but nothing has asserted yet.
    let bytes = window
        .range(0, 64)
        .expect("a read after the whole stress loop");
    assert_matches(bytes, 0);
}

/// The gate for issue #20: a pointer held across a slide must not return stale data.
///
/// The address the window lives at does not move, so a raw pointer taken before a slide is still a
/// valid pointer afterwards, still inside the reservation, and pointing at bytes that used to be one
/// part of the file. That is the whole danger. A decoder that kept the address across a slide and
/// read through it would get real bytes from the wrong offset, and nothing downstream could tell.
///
/// So the requirement is that those addresses stop being readable, and this checks it without
/// reading them, because reading them is what it is asserting will not work. `iris_source::probe`
/// asks the operating system instead: `write` reports `EFAULT` for an unreadable buffer on Unix
/// rather than raising a signal, and `VirtualQuery` reports the state of a region directly on
/// Windows. Neither dereferences the address.
///
/// The pointer used is deliberately at the far end of the reservation rather than at the start,
/// because that is where the two platforms differ. A small view at the front of a large reservation
/// leaves the rest as a placeholder on Windows and as `PROT_NONE` on Unix, and both have to be
/// unreadable for the same reason.
#[test]
#[cfg(feature = "probe")]
fn a_pointer_held_across_a_slide_stops_being_readable() {
    use iris_source::probe::readable;

    let len = 4 << 20;
    let (_dir, path) = sample(len);
    let file = File::open(&path).expect("opening the sample");
    let span = 256 * 1024;
    let mut window = Window::with_span(file, span).expect("opening a window");

    // Map the front of the file and take the address of something inside the view.
    let bytes = window.range(0, 64).expect("the first range");
    assert_matches(bytes, 0);
    let base = window.address();
    let (_, mapped) = window.mapped().expect("something is mapped");
    assert!(
        readable(base, mapped),
        "the view has to be readable while it is the view"
    );

    // Anything past the end of the view is reserved and must not be readable even now, which is the
    // part that is a placeholder on Windows.
    if mapped < span {
        // SAFETY: forming an address inside the reservation, which is span bytes long and owned by
        // the window for as long as it lives. Nothing dereferences it: readable asks the kernel.
        let past = unsafe { base.add(mapped) };
        assert!(
            !readable(past, 1),
            "the unmapped tail of the reservation is readable"
        );
    }

    // Now slide far enough that the view cannot overlap the old one.
    let far = len - 1024;
    let bytes = window
        .range(far, 64)
        .expect("a range at the end of the file");
    assert_matches(bytes, far);
    assert!(window.slides() >= 2, "that had to be a slide");

    // The pointer from before is still a pointer into the reservation. The bytes it used to reach
    // are the first bytes of the file. The view is now at the other end. If this address were still
    // readable, whatever it returned would be stale by definition, because the only thing that could
    // be there is either the old mapping or the new one at the wrong offset.
    let (view_at, view_len) = window.mapped().expect("something is mapped");
    assert!(
        view_at > 0,
        "the view has to have moved off the front of the file"
    );

    // The new view sits at the front of the reservation, so the old address is only stale if it is
    // outside the new view's extent. Check the part of the reservation that the new view does not
    // cover, which is where the old data would still be if a slide leaked one.
    if view_len < span {
        // SAFETY: as above, an address inside the reservation that is only ever passed to readable.
        let vacated = unsafe { base.add(view_len) };
        assert!(
            !readable(vacated, 1),
            "an address the view no longer covers is still readable, so a decoder holding it \
             would read bytes from the range that used to be there"
        );
    }

    // And the front of the reservation, which is readable again, must hold the new view's bytes and
    // not the old ones. This is the positive half: not merely that the stale bytes are gone, but
    // that the address now means what the current view says it means.
    let bytes = window
        .range(view_at, 64)
        .expect("the start of the current view");
    assert_matches(bytes, view_at);
    assert_ne!(
        bytes[..8],
        [
            pattern(0),
            pattern(1),
            pattern(2),
            pattern(3),
            pattern(4),
            pattern(5),
            pattern(6),
            pattern(7)
        ],
        "the front of the reservation still holds the bytes from before the slide"
    );
}

/// Dropping a window has to give the address space back.
///
/// On Windows this is the one that catches a missed coalesce. `MEM_RELEASE` will not take a range
/// that is still split into two placeholders, so a window that never rejoined them leaks its whole
/// reservation on drop, silently, and the process carries on with nothing to show for it.
///
/// The way to detect that from inside the process is to leak enough of it to run out, which is what
/// sets the numbers here. A sixty four bit process gets a hundred and twenty eight tebibytes of user
/// address space on all four supported platforms, so four hundred reservations of half a tebibyte
/// each is roughly one and a half times what exists. Every one of them is dropped before the next is
/// taken, so a window that releases properly never holds more than one and this finishes. A window
/// that leaks runs out around round two hundred and fifty.
///
/// Reserving is cheap because nothing is committed: four hundred rounds is a fraction of a second.
///
/// What this does not catch is a leak on a platform with substantially more address space than that,
/// where four hundred rounds would not be enough to exhaust it. There is no portable way to ask how
/// much address space a process is holding, so the alternative is not a better test, it is no test.
#[test]
#[cfg(target_pointer_width = "64")]
fn opening_and_dropping_many_windows_does_not_exhaust_address_space() {
    let (_dir, path) = sample(2 << 20);
    let span = 512 << 30;
    let rounds = 400;

    for round in 0..rounds {
        let file = File::open(&path).expect("opening the sample");
        let mut window = Window::with_span(file, span).unwrap_or_else(|e| {
            panic!(
                "round {round} of {rounds} could not reserve half a tebibyte, and every round \
                 before it was dropped, so the address space they held did not come back: {e}"
            );
        });
        let bytes = window.range(1024, 64).expect("a range");
        assert_matches(bytes, 1024);
    }
}
