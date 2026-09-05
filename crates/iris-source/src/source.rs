//! The trait a decoder's byte ranges are served through, and what an implementation has to promise.
//!
//! # The inversion
//!
//! A decoder does not open files and it does not make requests. It says which bytes it needs and the
//! host produces them. That is the whole reason this trait exists: I/O scheduling, caching, retries
//! and concurrency are decisions about the machine the query is running on, and a decoder shipped as
//! a sandboxed module knows nothing about that machine. Keeping the decision on the host side is
//! what lets the same decoder binary read a mapped local file and an object over HTTP without being
//! recompiled, which is the M4 gate in `docs/ROADMAP.md`.
//!
//! # Why asking does not wait
//!
//! [`RangeSource::range`] answers immediately. It either hands back the bytes or it says they are
//! not here yet, and it never blocks waiting for them. A blocking read would be simpler and it would
//! make a single threaded host impossible to write, because the one thread would sit in a syscall
//! while every other query on the machine waited behind it. So the answer is [`Fetch::Pending`] and
//! the caller goes and does something else, which is what the resumable `require_range` path in the
//! sandbox is built on.
//!
//! A host that genuinely does not mind blocking can call [`read_blocking`] and get the old shape
//! back in one line. That is a choice the host makes, which is the point.
//!
//! # What an implementation has to promise
//!
//! Five things, and the conformance suite checks all of them.
//!
//! **Bounds are an error, not a short read.** A range that runs past the end comes back as
//! [`SourceError::OutOfBounds`] rather than as the bytes that happen to exist. A decoder that asked
//! for the wrong thing should find out, and truncation is how a wrong length turns into a plausible
//! wrong answer.
//!
//! **A source stays usable after a refusal.** Asking for something out of bounds is a question, not
//! damage. The next valid range still has to work.
//!
//! **Ready is sticky.** If a range came back [`Fetch::Ready`], asking for that same range again
//! immediately has to come back ready with the same bytes. Without that, a caller cannot tell the
//! difference between a source that is making progress and one that is thrashing, and
//! [`read_blocking`] could not terminate.
//!
//! **Order does not matter.** The bytes for a range do not depend on which ranges were asked for
//! before it. A source is allowed to be much slower when the order is unhelpful, and it is not
//! allowed to be wrong.
//!
//! **Counters only go up.** [`RangeSource::traffic`] reports what the source has done since it was
//! opened, and it never reports less than it did a moment ago. A host measuring one scan takes the
//! difference across it, and a counter that can go backwards makes that difference meaningless.
//!
//! Nothing here says a source has to remember more than one range. A windowed file keeps whatever
//! its current view covers, an object source keeps its last block, and a memory source keeps
//! everything. All three satisfy the four promises above, which is what makes them substitutable.
//!
//! # Writing a fourth
//!
//! Implement [`len`](RangeSource::len), [`range`](RangeSource::range) and
//! [`traffic`](RangeSource::traffic), override [`largest`](RangeSource::largest) if a single call
//! cannot serve an arbitrarily long range, and run it through [`crate::conformance`] with the
//! `conformance` feature on. If the suite passes, the rest of iris will drive it.

use std::io;

/// What a source says when it is asked for a range.
///
/// This is deliberately two cases and not three. There is no "ready but only partly", because a
/// short read is the failure mode this whole design exists to remove.
#[derive(Debug)]
#[non_exhaustive]
pub enum Fetch<'a> {
    /// The bytes, exactly as many as were asked for.
    Ready(&'a [u8]),

    /// Not here yet. The source has started or continued whatever work is needed and the caller
    /// should come back, having done something useful in between.
    Pending,
}

impl Fetch<'_> {
    /// Whether this is [`Fetch::Ready`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Fetch::Ready(_))
    }
}

/// What can go wrong when a source is asked for bytes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The requested range runs past the end of the source.
    #[error("bytes {at}..{end} were asked for and the source is {len} bytes long")]
    OutOfBounds {
        /// Where the request started.
        at: u64,
        /// One past where it ended, saturated if the addition would have overflowed.
        end: u64,
        /// How many bytes the source holds.
        len: u64,
    },

    /// No single call to this source could serve a range that long.
    ///
    /// A configuration error rather than a data error. The caller asked a windowed source for more
    /// than the window holds, and the answer is to open it with a larger span or to ask in pieces.
    #[error("serving that range needs {wanted} bytes at once and this source can hold {largest}")]
    TooLarge {
        /// How many bytes one call would have to cover, including any alignment slack in front.
        wanted: usize,
        /// How many it can cover.
        largest: usize,
    },

    /// The operating system refused something.
    #[error("{operation} failed: {source}")]
    Io {
        /// Which step failed, so the message says where in the sequence it stopped.
        operation: &'static str,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },

    /// Fetching the bytes failed for a reason that belongs to the source rather than to iris.
    ///
    /// The boxed error is the underlying one, kept whole rather than flattened into a string,
    /// because the caller retrying a request wants to look at what it was.
    #[error("fetching bytes {at}..{end} failed: {source}")]
    Fetch {
        /// Where the request started.
        at: u64,
        /// One past where it ended.
        end: u64,
        /// What the source said.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A source said a range was ready and then said it was not.
    ///
    /// This is a bug in the source rather than a condition a caller can recover from, and it has a
    /// variant of its own so that the message names the contract that was broken instead of
    /// appearing as a hang.
    #[error("this source said bytes {at}..{end} were ready and then said they were not")]
    Flapped {
        /// Where the request started.
        at: u64,
        /// One past where it ended.
        end: u64,
    },
}

/// What a source has done to serve the ranges it has been asked for.
///
/// Wall clock hides the mechanism. Two scans that take the same time can differ by an order of
/// magnitude in how many round trips they made and how many bytes came back, and a design whose
/// central claim is that declaring ranges moves fewer bytes has to be able to show that directly
/// rather than leave it to be inferred from a duration.
///
/// Both counters run from when the source was opened and only ever go up, so what one scan cost is
/// the difference across it. [`Traffic::since`] is that subtraction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Traffic {
    /// How many times the source went to whatever is underneath it.
    ///
    /// One request over the network, one window slide, one read. A range served out of something
    /// the source was already holding costs nothing and is not counted, which is the whole reason
    /// for counting.
    pub requests: u64,

    /// How many bytes those trips brought within reach.
    ///
    /// For a source over a network this is bytes that crossed it. For a mapped file it is the span
    /// that was mapped, which is what the host asked the kernel to make addressable rather than
    /// what the kernel went on to read, because the host has no way to see the second number. So
    /// the two are comparable as how much the host asked for and not as how much moved.
    pub bytes: u64,
}

impl Traffic {
    /// Nothing fetched and nothing brought across.
    pub const NONE: Self = Self {
        requests: 0,
        bytes: 0,
    };

    /// What has happened since `earlier`.
    ///
    /// Saturating, so a source that broke the promise that counters only go up reports zero here
    /// rather than a number just short of `u64::MAX` that would read as a catastrophe.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            requests: self.requests.saturating_sub(earlier.requests),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// A place a decoder's byte ranges come from.
///
/// See the [module documentation](self) for the five promises an implementation makes and for how
/// to write one.
pub trait RangeSource {
    /// How many bytes the source holds.
    fn len(&self) -> u64;

    /// Whether the source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The longest range this source will serve in one call, whatever offset it starts at, or
    /// `None` if it is not bounded.
    ///
    /// A host sizing its requests wants the guaranteed number rather than the best case, so a
    /// windowed source subtracts its worst case alignment slack before answering. Asking for more
    /// than this is [`SourceError::TooLarge`] and not a partial answer.
    fn largest(&self) -> Option<usize> {
        None
    }

    /// Asks for `len` bytes starting at `at`, without waiting for them.
    ///
    /// Returns [`Fetch::Ready`] with exactly `len` bytes, or [`Fetch::Pending`] if the source has
    /// work outstanding. Pending is not a failure and the caller is expected to ask again.
    ///
    /// # Errors
    ///
    /// [`SourceError::OutOfBounds`] if the range leaves the source, [`SourceError::TooLarge`] if no
    /// single call could cover it, and an implementation specific error if the fetch itself failed.
    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError>;

    /// What this source has done since it was opened.
    ///
    /// Required rather than defaulted, which is a deliberate cost imposed on anyone writing a
    /// fourth implementation. A default would have to be zero, an implementation that never thought
    /// about the question would report zero, and zero is also the honest answer from a source that
    /// really did fetch nothing. Those two would then be indistinguishable at precisely the moment
    /// somebody is trying to find out where the bytes went.
    fn traffic(&self) -> Traffic;
}

/// Asks for a range and waits until it arrives.
///
/// This is the convenience for hosts that are allowed to block: a batch tool, a test, a program
/// with a thread to spare. It spins on [`RangeSource::range`], yielding to the scheduler between
/// tries, so the thread that calls it is not available for anything else until the bytes are there.
/// A host that cares about that should drive [`RangeSource::range`] itself and get on with other
/// work while the answer is [`Fetch::Pending`].
///
/// It terminates because ready is sticky. A source that goes ready and then goes pending again for
/// the same range has broken that promise, and this reports it as [`SourceError::Flapped`] rather
/// than spinning forever on it.
///
/// # Errors
///
/// Whatever [`RangeSource::range`] returns, plus [`SourceError::Flapped`] if the source contradicts
/// itself.
pub fn read_blocking(
    source: &mut dyn RangeSource,
    at: u64,
    len: usize,
) -> Result<&[u8], SourceError> {
    // Two phases rather than returning the slice from inside the loop. The borrow checker cannot
    // see that the borrow taken by a loop iteration which did not return is over, so the loop
    // establishes readiness without keeping the bytes and then one more call hands them back. The
    // second call costs a comparison, because the range it asks for is the one just made ready.
    while !source.range(at, len)?.is_ready() {
        std::thread::yield_now();
    }

    match source.range(at, len)? {
        Fetch::Ready(bytes) => Ok(bytes),
        Fetch::Pending => Err(SourceError::Flapped {
            at,
            end: at.saturating_add(len as u64),
        }),
    }
}

/// The bounds check every implementation runs, in one place so they cannot disagree about it.
///
/// Returns the end of the range on success. An addition that would overflow saturates, which then
/// fails the comparison, so an absurd request is out of bounds rather than a panic or a wrap.
///
/// Public because it is the first thing a fourth implementation has to get right and there is no
/// reason for anyone to write it again. Call it before anything else in
/// [`range`](RangeSource::range) and the conformance suite's bounds checks pass by construction.
///
/// # Errors
///
/// [`SourceError::OutOfBounds`] if the range does not fit in `have` bytes.
pub fn bounds(at: u64, len: usize, have: u64) -> Result<u64, SourceError> {
    let end = at.saturating_add(len as u64);
    if end > have {
        return Err(SourceError::OutOfBounds { at, end, len: have });
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_that_fits_reports_where_it_ends() {
        assert!(matches!(bounds(0, 10, 10), Ok(10)));
        assert!(matches!(bounds(10, 0, 10), Ok(10)));
        assert!(matches!(bounds(3, 4, 10), Ok(7)));
    }

    #[test]
    fn a_range_that_leaves_the_source_is_out_of_bounds_rather_than_a_panic() {
        assert!(matches!(
            bounds(10, 1, 10),
            Err(SourceError::OutOfBounds {
                at: 10,
                end: 11,
                len: 10
            })
        ));
        // The addition saturates instead of wrapping, so this fails the comparison rather than
        // coming out as a small end that looks like it fits.
        assert!(matches!(
            bounds(u64::MAX, usize::MAX, 10),
            Err(SourceError::OutOfBounds { .. })
        ));
    }
}
