//! A source over an object store, which is the implementation the pending case exists for.
//!
//! The other two sources always answer straight away. This one cannot: the bytes are at the other
//! end of a network and getting them takes milliseconds that the calling thread should not spend
//! waiting. So a request is spawned onto a runtime the host already has, and every call to
//! [`RangeSource::range`] in the meantime says [`Fetch::Pending`] and returns immediately.
//!
//! # Why a channel and not a future
//!
//! Holding the future itself would mean this type had to be polled with a waker, which would make
//! every caller async and would push the choice of runtime into the decoder side of the boundary.
//! Instead the spawned task sends its result down a plain channel and `range` calls `try_recv`,
//! which is the one operation that asks "is it here" without agreeing to wait. The cost is one
//! allocation and one channel per request. The gain is that a synchronous host, which is what the
//! sandbox side is, can drive this without knowing what a future is.
//!
//! # One block at a time
//!
//! This keeps whatever it last fetched and nothing else. A request that falls inside the held block
//! is free, and one that does not starts a fetch and drops any request already in flight for a
//! different range. That is the smallest thing that satisfies the trait, and it is deliberately not
//! a cache: coalescing neighbouring requests and prefetching ahead of a scan are host side policy,
//! they are worth measuring rather than guessing, and they are issue #28.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, path::Path};
use tokio::runtime::Handle;

use crate::source::{Fetch, RangeSource, SourceError, bounds};

/// A [`RangeSource`] over one object in an object store.
#[derive(Debug)]
pub struct ObjectSource {
    store: Arc<dyn ObjectStore>,
    path: Path,
    runtime: Handle,
    len: u64,
    held: Option<Held>,
    inflight: Option<Inflight>,
    requests: u64,
    transferred: u64,
}

/// The block this source last fetched, and where in the object it came from.
#[derive(Debug)]
struct Held {
    at: u64,
    bytes: Bytes,
}

impl Held {
    fn covers(&self, at: u64, len: u64) -> bool {
        let Some(want_end) = at.checked_add(len) else {
            return false;
        };
        at >= self.at && want_end <= self.at + self.bytes.len() as u64
    }
}

/// A request that has been spawned and has not come back.
#[derive(Debug)]
struct Inflight {
    at: u64,
    len: usize,
    done: Receiver<object_store::Result<Bytes>>,
}

/// Whether a fetch has landed, used to keep the borrow of `held` out of the code that mutates it.
enum Progress {
    Arrived,
    Waiting,
}

impl ObjectSource {
    /// Opens the object at `path`, asking the store how long it is.
    ///
    /// The length is needed before any range can be bounds checked, and the only way to learn it is
    /// a request, so this is the one asynchronous step. Everything after it is synchronous. A host
    /// that already knows the length from a catalogue should call [`ObjectSource::with_len`] and
    /// skip the round trip.
    ///
    /// # Errors
    ///
    /// If the store cannot describe the object.
    pub async fn open(store: Arc<dyn ObjectStore>, path: Path) -> Result<Self, SourceError> {
        let meta = store
            .head(&path)
            .await
            .map_err(|source| SourceError::Fetch {
                at: 0,
                end: 0,
                source: Box::new(source),
            })?;
        let len = meta.size;
        Ok(Self::with_len(store, path, len, Handle::current()))
    }

    /// Builds a source for an object whose length is already known.
    ///
    /// `runtime` is where fetches are spawned. It is taken rather than created because starting a
    /// runtime is a decision about how the whole program is scheduled, and a library that makes it
    /// on the caller's behalf has taken something that was not offered.
    #[must_use]
    pub fn with_len(store: Arc<dyn ObjectStore>, path: Path, len: u64, runtime: Handle) -> Self {
        Self {
            store,
            path,
            runtime,
            len,
            held: None,
            inflight: None,
            requests: 0,
            transferred: 0,
        }
    }

    /// How many requests this source has sent to the store.
    ///
    /// Wall clock alone hides the mechanism. Two scans that take the same time can differ by an
    /// order of magnitude in how many round trips they made, and this is the number that says
    /// which one was lucky.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// How many bytes have arrived from the store.
    ///
    /// Counted on arrival rather than on request, so a fetch that failed does not appear as traffic
    /// that never happened.
    #[must_use]
    pub fn transferred(&self) -> u64 {
        self.transferred
    }

    /// The path this source reads.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Starts a fetch, replacing anything already in flight.
    ///
    /// Dropping the receiver for a previous request does not cancel the task, which will finish and
    /// find nobody listening. That is the honest cost of not holding the future: a request already
    /// paid for is not recovered. It only happens when a caller abandons a range part way through,
    /// which a scan does not do.
    fn start(&mut self, at: u64, len: usize) {
        let (sender, done) = std::sync::mpsc::channel();
        let store = Arc::clone(&self.store);
        let path = self.path.clone();
        let range = at..at + len as u64;

        self.runtime.spawn(async move {
            // The receiver is gone if the caller moved on. Nothing to report and nobody to report
            // it to, so the result is dropped rather than logged, which would be a line of noise
            // per abandoned request.
            let _ = sender.send(store.get_range(&path, range).await);
        });

        self.requests += 1;
        self.inflight = Some(Inflight { at, len, done });
    }

    /// Moves the outstanding request along by one step, starting it if there is not one already.
    fn progress(&mut self, at: u64, len: usize, end: u64) -> Result<Progress, SourceError> {
        let matching = self
            .inflight
            .as_ref()
            .is_some_and(|inflight| inflight.at == at && inflight.len == len);
        if !matching {
            self.start(at, len);
            return Ok(Progress::Waiting);
        }

        let inflight = self.inflight.as_ref().expect("just checked there is one");
        let arrived = match inflight.done.try_recv() {
            Ok(arrived) => arrived,
            Err(TryRecvError::Empty) => return Ok(Progress::Waiting),
            // The task ended without sending, which means it was cancelled or the runtime is
            // shutting down. Neither is something to retry into.
            Err(TryRecvError::Disconnected) => {
                self.inflight = None;
                return Err(SourceError::Fetch {
                    at,
                    end,
                    source: "the fetch task ended without answering".into(),
                });
            }
        };
        self.inflight = None;

        let bytes = arrived.map_err(|source| SourceError::Fetch {
            at,
            end,
            source: Box::new(source),
        })?;

        // A store that answered with fewer bytes than were asked for inside a range it accepted has
        // broken its own contract, and passing that on as a short slice is how a wrong length turns
        // into a plausible wrong answer further down.
        if bytes.len() != len {
            return Err(SourceError::Fetch {
                at,
                end,
                source: format!(
                    "the store returned {} bytes and {len} were asked for",
                    bytes.len()
                )
                .into(),
            });
        }

        self.transferred += bytes.len() as u64;
        self.held = Some(Held { at, bytes });
        Ok(Progress::Arrived)
    }
}

impl RangeSource for ObjectSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        let end = bounds(at, len, self.len)?;

        // No request is worth a round trip for no bytes, and a store asked for an empty range is
        // entitled to refuse. The empty slice is the right answer and it is free.
        if len == 0 {
            return Ok(Fetch::Ready(&[]));
        }

        let held = self
            .held
            .as_ref()
            .is_some_and(|held| held.covers(at, len as u64));
        if !held && matches!(self.progress(at, len, end)?, Progress::Waiting) {
            return Ok(Fetch::Pending);
        }

        let held = self
            .held
            .as_ref()
            .expect("the block just arrived or was already here");
        // `at` is inside the held block by the check above, so the difference fits in the block's
        // own length, which is a usize.
        let offset = usize::try_from(at - held.at).unwrap_or(usize::MAX);
        Ok(Fetch::Ready(&held.bytes[offset..offset + len]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_block_covers_what_is_inside_it_and_nothing_else() {
        let held = Held {
            at: 100,
            bytes: Bytes::from_static(&[0; 50]),
        };
        assert!(held.covers(100, 50));
        assert!(held.covers(120, 10));
        assert!(held.covers(150, 0));
        assert!(!held.covers(99, 1));
        assert!(!held.covers(150, 1));
        assert!(!held.covers(u64::MAX, 2));
    }
}
