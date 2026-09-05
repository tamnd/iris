//! The suite that turns [`RangeSource`] from a shape into a contract.
//!
//! A trait with three implementations in one repository tends to mean whatever the three of them
//! happen to do. The point of shipping the checks in the library rather than in this crate's tests
//! is that a fourth implementation, written by somebody else in another repository, can be held to
//! exactly the same promises without copying anything.
//!
//! Turn the `conformance` feature on and call [`check`]. It panics on the first failure with a
//! message naming the promise that was broken, because it is meant to be called from a test.
//!
//! # What it does not check
//!
//! Speed, memory, and how many times the source went to the network. Those are properties worth
//! measuring and they are not properties of correctness, and a suite that mixed the two would fail
//! on a slow machine and be ignored from then on.
//!
//! It also does not check thread safety, because [`RangeSource`] does not promise any. A source is
//! driven by whichever thread owns it.

use crate::source::{Fetch, RangeSource, SourceError, read_blocking};

/// Runs every conformance check against `source`, which must hold exactly `contents`.
///
/// `contents` is the truth the source is compared against, so this is only useful where the caller
/// knows what the source should be serving. That is the normal case for a test: build the bytes,
/// hand them to the implementation, hand the same bytes to this.
///
/// # Panics
///
/// On the first broken promise, with a message saying which one and what the source did instead.
pub fn check(source: &mut dyn RangeSource, contents: &[u8]) {
    let len = contents.len() as u64;

    assert_eq!(
        source.len(),
        len,
        "a source has to report the length of what it holds"
    );
    assert_eq!(
        source.is_empty(),
        contents.is_empty(),
        "is_empty has to agree with len"
    );

    let largest = source
        .largest()
        .unwrap_or(contents.len())
        .min(contents.len());

    for at in offsets(len) {
        for want in lengths(largest) {
            let Some(end) = at.checked_add(want as u64) else {
                continue;
            };
            if end > len {
                continue;
            }

            let bytes = read_blocking(source, at, want).unwrap_or_else(|error| {
                panic!("reading {want} bytes at {at} of {len} should have worked: {error}")
            });
            // The comparison above put `end` inside the contents, so the offset fits whatever
            // width a pointer is on this target.
            let start = usize::try_from(at).expect("an offset inside the contents");
            assert_eq!(
                bytes,
                &contents[start..start + want],
                "the bytes for {want} at {at} are not the ones in the source"
            );
        }
    }

    // The order these run in is not arbitrary. The loop above reads forwards, so it is the first
    // check below that reads backwards which catches a source whose view only moves one way, and
    // whichever check that is gets blamed for it. Putting the direction check immediately after the
    // forwards pass means the failure is reported by the check that is actually about direction,
    // rather than by whichever later check happened to look back first.
    order_does_not_matter(source, contents, largest);
    zero_length_at_the_end(source, len);
    ready_is_sticky(source, contents);
    out_of_bounds_does_not_break_the_source(source, contents);
    the_largest_promised_range_is_served_wherever_it_starts(source, contents, largest);
}

/// Offsets worth trying, which are the ones near a boundary of some kind.
///
/// A source that is wrong is almost never wrong in the middle of a page. It is wrong at zero, at
/// the end, one byte either side of an alignment unit, and at the point where a window has to move.
fn offsets(len: u64) -> Vec<u64> {
    let mut offsets = vec![0, len / 3, len / 2, len];
    for boundary in [4096_u64, 16384, 65536] {
        offsets.extend([boundary.wrapping_sub(1), boundary, boundary.wrapping_add(1)]);
    }
    if len > 0 {
        offsets.push(len - 1);
    }
    offsets.retain(|&at| at <= len);
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

/// Lengths worth trying, for a source that promises to serve at most `largest` in one call.
fn lengths(largest: usize) -> Vec<usize> {
    let mut lengths = vec![0, 1, 2, 3, 7, 4095, 4096, 4097, 16384, 65536, largest];
    lengths.retain(|&len| len <= largest);
    lengths.sort_unstable();
    lengths.dedup();
    lengths
}

/// A zero length range at exactly the end is a real request and it has a real answer.
///
/// This is the shape a decoder produces when it asks for a column that happens to be empty and sits
/// last in the file. It looks like nothing from inside the arithmetic and it is where the window
/// implementation first went wrong.
fn zero_length_at_the_end(source: &mut dyn RangeSource, len: u64) {
    let bytes = read_blocking(source, len, 0)
        .unwrap_or_else(|error| panic!("a zero length range at the end is not an error: {error}"));
    assert!(bytes.is_empty(), "a zero length range has no bytes in it");
}

/// A range that leaves the source is refused, and refusing it does not damage anything.
fn out_of_bounds_does_not_break_the_source(source: &mut dyn RangeSource, contents: &[u8]) {
    let len = contents.len() as u64;

    let refused = source.range(len, 1);
    assert!(
        matches!(refused, Err(SourceError::OutOfBounds { .. })),
        "one byte past the end has to be out of bounds rather than a short read"
    );

    // Not a panic and not a wrap. A caller that computed an absurd offset should get an error back
    // and be able to carry on.
    let refused = source.range(u64::MAX, usize::MAX);
    assert!(
        refused.is_err(),
        "an offset and length that would overflow has to be an error"
    );

    if !contents.is_empty() {
        let bytes = read_blocking(source, 0, 1)
            .unwrap_or_else(|error| panic!("a source has to survive being refused: {error}"));
        assert_eq!(bytes, &contents[..1], "and still serve the right bytes");
    }
}

/// A range that came back ready comes back ready again, with the same bytes.
///
/// This is what makes a caller able to tell progress from thrashing, and it is what lets
/// [`read_blocking`] terminate.
fn ready_is_sticky(source: &mut dyn RangeSource, contents: &[u8]) {
    if contents.is_empty() {
        return;
    }

    let want = contents.len().min(64);
    read_blocking(source, 0, want).expect("the first bytes are readable");

    match source.range(0, want) {
        Ok(Fetch::Ready(bytes)) => assert_eq!(
            bytes,
            &contents[..want],
            "asking again gave different bytes"
        ),
        Ok(Fetch::Pending) => panic!("a range that was ready went back to pending"),
        Err(error) => panic!("a range that was ready came back as an error: {error}"),
    }
}

/// The bytes for a range do not depend on which ranges were asked for before it.
///
/// The failure this catches is a source that only moves forwards, which passes every check that
/// happens to read in order and then returns the wrong bytes the first time a decoder revisits a
/// footer.
fn order_does_not_matter(source: &mut dyn RangeSource, contents: &[u8], largest: usize) {
    let len = contents.len();
    if len == 0 {
        return;
    }

    let want = largest.min(len).clamp(1, 1024);
    let stride = len / 7 + 1;

    // Forwards, then backwards over the same offsets. Two passes rather than a shuffle, because a
    // shuffle needs a generator and the failure being looked for is direction, not randomness.
    let mut offsets: Vec<usize> = (0..len)
        .step_by(stride)
        .filter(|&at| at + want <= len)
        .collect();
    offsets.extend(offsets.clone().into_iter().rev());

    for at in offsets {
        let bytes = read_blocking(source, at as u64, want).unwrap_or_else(|error| {
            panic!("reading {want} bytes at {at} should have worked: {error}")
        });
        assert_eq!(
            bytes,
            &contents[at..at + want],
            "reading in this order gave the wrong bytes at {at}"
        );
    }
}

/// A range of exactly the promised length works wherever it starts.
///
/// The promise [`RangeSource::largest`] makes is not about the best case. A windowed source whose
/// answer was the whole span would pass every aligned request and fail the misaligned ones, which is
/// a bug that only appears once the data changes shape.
fn the_largest_promised_range_is_served_wherever_it_starts(
    source: &mut dyn RangeSource,
    contents: &[u8],
    largest: usize,
) {
    let len = contents.len();
    if largest == 0 || largest > len {
        return;
    }

    for at in [0, 1, 7, 4095, len - largest] {
        if at + largest > len {
            continue;
        }
        let bytes = read_blocking(source, at as u64, largest).unwrap_or_else(|error| {
            panic!("the promised length of {largest} was refused at offset {at}: {error}")
        });
        assert_eq!(
            bytes,
            &contents[at..at + largest],
            "the promised length gave the wrong bytes at {at}"
        );
    }
}
