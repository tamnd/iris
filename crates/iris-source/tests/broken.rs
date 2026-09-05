//! Sources that are wrong on purpose, so that the conformance suite is known to bite.
//!
//! A suite that every implementation passes is worth exactly as much as the failures it would have
//! caught, and the only way to know that number is not zero is to write implementations that are
//! wrong in the ways a real one would be wrong and watch the suite reject them.
//!
//! These are also the clearest worked examples of writing a fourth source. Each one is about twenty
//! lines and the bug in it is a single line, which is roughly how a real one arrives.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the corpus pattern is a byte by construction, and every offset here has been bounds \
              checked against a buffer that is in memory"
)]

use std::panic::AssertUnwindSafe;

use iris_source::{Fetch, RangeSource, SourceError, Traffic, bounds, conformance};

const CORPUS: usize = 40_000;

fn corpus() -> Vec<u8> {
    (0..CORPUS).map(|at| (at % 251) as u8).collect()
}

/// Runs the suite and returns what it complained about, failing the test if it did not complain.
fn rejected(source: &mut dyn RangeSource, contents: &[u8]) -> String {
    // The default hook prints a backtrace for every one of these, which is a screen of noise for a
    // panic the test is asking for. It goes back afterwards so a genuine failure elsewhere still
    // prints normally.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        conformance::check(source, contents);
    }));
    std::panic::set_hook(hook);

    let panic = outcome.expect_err("the suite should have rejected this source");
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "the suite panicked without a message".to_owned())
}

/// A source whose view only ever moves forwards.
///
/// The realistic bug. A scan reads in order, so this is right for every request a scan makes, and it
/// returns bytes from the wrong part of the file the first time a decoder goes back for a footer.
/// The bytes it returns are real bytes from a real offset, which is what makes it dangerous: there
/// is nothing about the answer that looks wrong.
struct NeverGoesBack {
    bytes: Vec<u8>,
    furthest: u64,
}

impl RangeSource for NeverGoesBack {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;
        self.furthest = self.furthest.max(at);

        // The bug, and it is one line: the view is where it got to rather than where it was asked
        // to be. Clamped so it stays inside the buffer, which is exactly what a real window's
        // arithmetic would do.
        let start = (self.furthest as usize).min(self.bytes.len() - len);
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        Traffic::NONE
    }
}

/// A source that says a range is ready and then says it is not.
///
/// The shape of a source that starts a fresh fetch on every call instead of noticing it already has
/// the bytes. Left alone it makes a caller loop forever, which is why the trait names the promise
/// and the suite checks it rather than waiting to find out in production.
struct Flapper {
    bytes: Vec<u8>,
    ready: bool,
}

impl RangeSource for Flapper {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;
        self.ready = !self.ready;
        if !self.ready {
            return Ok(Fetch::Pending);
        }
        let start = at as usize;
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        Traffic::NONE
    }
}

/// A source that serves what it has instead of refusing what it does not.
///
/// The classic short read, wearing the clothes of a helpful implementation. Every caller downstream
/// then has to check the length it got back against the length it asked for, and the first one that
/// forgets produces an answer that is wrong and looks right.
struct Lenient {
    bytes: Vec<u8>,
}

impl RangeSource for Lenient {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        let start = (at as usize).min(self.bytes.len());
        let end = start.saturating_add(len).min(self.bytes.len());
        Ok(Fetch::Ready(&self.bytes[start..end]))
    }

    fn traffic(&self) -> Traffic {
        Traffic::NONE
    }
}

/// A source that counts what each range cost rather than what every range has cost.
///
/// The natural mistake, and the reason the trait says what it says instead of leaving it to
/// whoever writes the fourth implementation. Reporting the last request is a perfectly sensible
/// thing for a source to want to say, and it is not what a host is asking. A host takes a reading
/// before a scan and a reading after it and subtracts, and against this source that subtraction
/// comes out as zero or as nonsense depending on which way the last two requests happened to fall.
struct Forgetful {
    bytes: Vec<u8>,
    last: u64,
}

impl RangeSource for Forgetful {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;
        // The bug, and it is one character: assignment where the real one accumulates.
        self.last = len as u64;
        let start = at as usize;
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }

    fn traffic(&self) -> Traffic {
        Traffic {
            requests: 1,
            bytes: self.last,
        }
    }
}

#[test]
fn a_source_that_only_moves_forwards_is_rejected() {
    let contents = corpus();
    let mut source = NeverGoesBack {
        bytes: contents.clone(),
        furthest: 0,
    };
    let complaint = rejected(&mut source, &contents);
    assert!(
        complaint.contains("order"),
        "the suite should have named the order it read in, and said: {complaint}"
    );
}

#[test]
fn a_source_that_takes_readiness_back_is_rejected() {
    let contents = corpus();
    let mut source = Flapper {
        bytes: contents.clone(),
        ready: false,
    };
    let complaint = rejected(&mut source, &contents);
    assert!(
        complaint.contains("pending") || complaint.contains("ready"),
        "the suite should have named the readiness promise, and said: {complaint}"
    );
}

#[test]
fn a_source_that_serves_a_short_read_instead_of_refusing_is_rejected() {
    let contents = corpus();
    let mut source = Lenient {
        bytes: contents.clone(),
    };
    let complaint = rejected(&mut source, &contents);
    assert!(
        complaint.contains("out of bounds"),
        "the suite should have named the bounds promise, and said: {complaint}"
    );
}

#[test]
fn a_source_whose_counters_go_backwards_is_rejected() {
    let contents = corpus();
    let mut source = Forgetful {
        bytes: contents.clone(),
        last: 0,
    };
    let complaint = rejected(&mut source, &contents);
    assert!(
        complaint.contains("traffic"),
        "the suite should have named the traffic promise, and said: {complaint}"
    );
}
