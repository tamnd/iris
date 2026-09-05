//! Driving a call that is allowed to stop in the middle.
//!
//! # Why a call into a decoder is not a function call
//!
//! A decoder that pulls its own bytes will ask for a range the host does not have yet. The host has
//! three things it could do about that. It could block the thread until the bytes arrive, which
//! costs one thread per query in flight and is the reason database engines do not do it. It could
//! abandon the call and start it again once the bytes are there, which throws away everything the
//! decoder had decoded and cannot terminate if the next range misses too. Or it could stop the
//! decoder where it stands, keep its stack, and carry on later.
//!
//! This is the third one. [`Running`] is a call that has started and has not finished, and
//! [`Running::poll`] moves it as far as it will go right now. What comes back is either the answer
//! or [`Progress::Suspended`], which means the decoder is waiting on bytes and the thread is free.
//! Nothing is lost in between: the guest's stack, its locals, its allocations and every row it has
//! already decoded are exactly where it left them, because the call was suspended rather than
//! unwound.
//!
//! The number of times a call may suspend is not bounded. A scan over an object store that misses on
//! every one of ten thousand ranges suspends ten thousand times, and the ten thousandth resumption
//! costs what the first one did.
//!
//! # For a host that does not mind waiting
//!
//! [`Running::wait`] drives the call to the end and hands back the answer. It spins, so the thread
//! that calls it is not doing anything else until the decoder is done, and that is the point: a host
//! that wants the simple shape asks for it in one word at the call site rather than getting it by
//! default.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::error::Result;

/// How far a call into a decoder got.
///
/// Two variants and no room for a third, which is why this is not marked as open to additions. A
/// call either produced its answer or it did not, and a host matching on it should not have to write
/// an arm for a case that cannot exist in order to compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress<T> {
    /// The call finished and this is what it produced.
    Done(T),

    /// The decoder asked for bytes that were not there yet and has been stopped where it stood.
    ///
    /// Nothing has been lost and nothing needs to be replayed. Serve the range, or wait for whatever
    /// is fetching it, and poll again.
    Suspended,
}

/// A call into a decoder that has started and has not finished.
///
/// The handle borrows the decoder, so a second call cannot start while this one is halfway through.
/// That is not a limitation being worked around: a decoder has one stack and one set of buffers, and
/// two scans interleaved on them would be two scans reading each other's memory.
pub struct Running<'a, T> {
    call: Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
}

impl<T> core::fmt::Debug for Running<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Running").finish_non_exhaustive()
    }
}

impl<'a, T> Running<'a, T> {
    pub(crate) fn new(call: impl Future<Output = Result<T>> + 'a) -> Self {
        Self {
            call: Box::pin(call),
        }
    }

    /// Runs the decoder until it finishes or asks for bytes that are not there.
    ///
    /// # Errors
    ///
    /// Whatever the call itself returns. A suspension is not an error and never becomes one, however
    /// many times it happens.
    pub fn poll(&mut self) -> Result<Progress<T>> {
        // A waker that does nothing, because nothing here is waiting to be woken. The host is the
        // scheduler: it polls when it has reason to think the answer changed, and the source it
        // attached is the thing that knows whether it did. Handing a real waker down would mean
        // this crate deciding which executor a host runs, which is exactly the decision that stays
        // above it.
        let mut cx = Context::from_waker(Waker::noop());
        match self.call.as_mut().poll(&mut cx) {
            Poll::Ready(answer) => answer.map(Progress::Done),
            Poll::Pending => Ok(Progress::Suspended),
        }
    }

    /// Runs the decoder to the end, waiting for every range it asks for.
    ///
    /// This is the convenience for a host with a thread to spare: a batch tool, a test, anything
    /// reading a local file where a range never misses in the first place. It spins between polls,
    /// so the calling thread is not available for anything else until the answer is there.
    ///
    /// # Errors
    ///
    /// Whatever the call itself returns.
    pub fn wait(mut self) -> Result<T> {
        loop {
            match self.poll()? {
                Progress::Done(answer) => return Ok(answer),
                Progress::Suspended => std::thread::yield_now(),
            }
        }
    }
}

/// Runs a call that has no way to suspend, on a thread that is already committed to it.
///
/// Wasmtime treats asynchrony as a property of the store rather than of the call, so the moment one
/// import can suspend, every entry into the guest goes through the asynchronous door. That is right
/// for a scan and beside the point for the three calls that set one up: instantiating a module, and
/// asking the guest for room to put a record or a source in. None of them can reach
/// `iris.require_range`, so none of them can park, and making a host poll for an answer that is
/// already there would be a worse interface bought with nothing.
///
/// The loop is there for the case that cannot happen rather than the case that can. A module that
/// imports ranges and calls for one from inside its own allocator would suspend here, and then this
/// spins until the source produces the bytes, which is [`Running::wait`] with a different caller.
pub(crate) fn settled<T>(call: impl Future<Output = T>) -> T {
    let mut call = Box::pin(call);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(answer) = call.as_mut().poll(&mut cx) {
            return answer;
        }
        std::thread::yield_now();
    }
}

/// A future that gives the thread back once and is then done.
///
/// This is what turns a source saying "not yet" into the call suspending. Returning `Pending` from
/// inside a host import unwinds nothing: Wasmtime parks the guest's stack and hands control back to
/// whoever polled [`Running`], and polling again resumes the guest at the instruction after the
/// import call.
pub(crate) struct Yield {
    given: bool,
}

impl Yield {
    pub(crate) const fn once() -> Self {
        Self { given: false }
    }
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.given {
            return Poll::Ready(());
        }
        self.given = true;
        // Woken before it is parked, so this is a yield rather than a wait. A host driving
        // [`Running::poll`] itself ignores the waker, and a host that put this on a real executor
        // gets a task that is rescheduled instead of one that never runs again.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
