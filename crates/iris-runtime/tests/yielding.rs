//! The M6 gate on I/O: a decoder waiting for bytes does not cost a worker thread.
//!
//! A scan over an object store misses, and a miss is tens of milliseconds. A host that spends a
//! worker thread on each one has a query engine whose parallelism is bounded by how many round trips
//! are outstanding rather than by how much work there is, which on a fixed size pool is the
//! difference between a fast scan and a stalled one.
//!
//! The property is awkward to observe from the outside, because a host that holds the thread and a
//! host that gives it back both return the same rows. So the source here is a gate rather than a
//! stopwatch. Every scan is refused its first range and parks, and the gate does not open until all
//! of them have parked at once. If a scan held a worker while it waited, only as many scans as there
//! are workers would ever reach the gate and it would never open, so the test hangs rather than
//! returning the wrong number. That is what the watchdog is for.
//!
//! The second thing checked is how many times each source was asked. A host that gives the thread
//! back by handing the task straight to the scheduler again is not holding a worker either, and it
//! is still burning a core asking a question whose answer has not changed. A source that took a
//! waker is asked exactly once more, when the bytes are there, and the count says which of the two
//! is happening.

#![cfg(not(loom))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::thread;
use std::time::{Duration, Instant};

use iris_runtime::Runtime;
use iris_source::{Fetch, MemorySource, RangeSource, SourceError, Traffic, bounds};

mod support;

use support::{builder, column_values};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// More than one, so a decoder that asks for a single range is not what is being measured.
const COLUMNS: u64 = 2;

/// Few enough that the count of scans below cannot be served by one worker each.
const WORKERS: usize = 2;

/// Many more scans than there are workers, which is the whole point of the arrangement.
const SCANS: usize = 32;

/// How long the watchdog waits before calling it a hang.
///
/// Generous, because the failure it is guarding against is a deadlock and not a slow machine. A
/// scan of five hundred rows out of memory takes milliseconds, so anything near this number means
/// the workers are not coming back.
const PATIENCE: Duration = Duration::from_secs(60);

/// The thing every source in a run is waiting on.
///
/// It opens once, when as many sources have parked on it as there are scans, and it stays open. That
/// is deliberately not a barrier that resets: what is being checked is that all of them can be
/// waiting at the same moment, and once that has been shown there is nothing further to learn from
/// making them wait again.
#[derive(Debug)]
struct Gate {
    /// How many have to be waiting before any of them may go on.
    needed: usize,
    /// Whether the gate is in the way yet. Opening a container reads a trailer, a header, a footer
    /// and a decoder module, and stalling any of that would be measuring the wrong thing.
    armed: AtomicBool,
    /// How many times a source answered that the bytes were not there.
    pending: AtomicU64,
    party: Mutex<Party>,
}

#[derive(Debug, Default)]
struct Party {
    open: bool,
    waiting: Vec<Waker>,
}

impl Gate {
    fn new(needed: usize) -> Self {
        Self {
            needed,
            armed: AtomicBool::new(false),
            pending: AtomicU64::new(0),
            party: Mutex::new(Party::default()),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn in_the_way(&self) -> bool {
        self.armed.load(Ordering::SeqCst) && !self.party().open
    }

    fn saw_pending(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    fn pending_answers(&self) -> u64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// Adds a waker to the party, and opens the gate if that was the last one expected.
    ///
    /// Always true, because this gate is always going to fire the waker it was handed. The one that
    /// opens the gate wakes itself along with everybody else, which is the ordinary way a task that
    /// is about to return pending gets to be polled again.
    fn park(&self, waker: &Waker) -> bool {
        let mut party = self.party();
        if party.open {
            return false;
        }
        party.waiting.push(waker.clone());
        if party.waiting.len() < self.needed {
            return true;
        }

        party.open = true;
        let waiting = std::mem::take(&mut party.waiting);
        // Outside the lock, because a woken task can start running before wake returns and the first
        // thing it will do is ask its source for the range again, which comes back here.
        drop(party);
        for waker in waiting {
            waker.wake();
        }
        true
    }

    fn party(&self) -> MutexGuard<'_, Party> {
        self.party.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A source whose bytes are all in memory and which refuses to admit it until the gate opens.
///
/// Standing in for an object store, and a fair stand in for the part that matters. What a host can
/// see of a slow source is that it says the bytes are not here yet and later says they are, and this
/// says exactly that with the timing under the test's control rather than the network's.
#[derive(Debug)]
struct Stalling {
    inner: MemorySource,
    gate: Arc<Gate>,
}

impl RangeSource for Stalling {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        if !self.gate.in_the_way() {
            return self.inner.range(at, len);
        }

        // Bounds are checked even on the path that stalls. A range that leaves the source is a
        // decoder bug and it should be refused now rather than parked on a gate that would then
        // never see the number of waiters it is expecting.
        bounds(at, len, self.inner.len())?;
        self.gate.saw_pending();
        Ok(Fetch::Pending)
    }

    fn wake_when_ready(&mut self, waker: &Waker) -> bool {
        self.gate.park(waker)
    }

    fn traffic(&self) -> Traffic {
        self.inner.traffic()
    }
}

fn stalling(bytes: &[u8], gate: &Arc<Gate>) -> Stalling {
    Stalling {
        inner: MemorySource::new(bytes.to_vec()),
        gate: Arc::clone(gate),
    }
}

/// Runs `work` on a thread of its own and gives up on it after [`PATIENCE`].
///
/// Every way of getting this wrong ends in a hang rather than in a wrong answer, so the watchdog is
/// the only thing that turns either test into a failure somebody can read. It is a thread and a
/// clock rather than a timer on the executor being tested, because a worker that is held is holding
/// whatever else that executor had to run, and a test whose only way of reporting a stuck pool is a
/// timer that pool has to fire will hang instead of saying so.
fn within<T: Send + 'static>(complaint: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let running = thread::spawn(work);
    let deadline = Instant::now() + PATIENCE;
    while !running.is_finished() {
        assert!(Instant::now() < deadline, "{complaint}");
        thread::sleep(Duration::from_millis(10));
    }
    running.join().expect("the work does not panic")
}

#[test]
fn thirty_two_scans_all_waiting_at_once_do_not_need_thirty_two_workers() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("the container is writable");
    let runtime = Runtime::new().expect("a runtime starts");
    let gate = Arc::new(Gate::new(SCANS));

    let mut scans = Vec::with_capacity(SCANS);
    for _ in 0..SCANS {
        scans.push(
            runtime
                .open_windowed(Box::new(stalling(&bytes, &gate)))
                .expect("the container opens"),
        );
    }
    gate.arm();

    let complaint = format!(
        "the gate never opened, so fewer than {SCANS} scans were ever waiting at the same time. \
         That is what it looks like when a scan holds its worker thread across a miss: only \
         {WORKERS} of them start and the rest never run"
    );
    let rows = within(&complaint, move || {
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .build()
            .expect("an executor with a fixed size pool");
        executor.block_on(async move {
            let tasks: Vec<_> = scans
                .into_iter()
                .map(|mut windowed| {
                    tokio::spawn(async move {
                        let batches = windowed.scan_async().await.expect("the scan runs");
                        column_values(&batches, 0).len()
                    })
                })
                .collect();

            let mut rows = Vec::with_capacity(tasks.len());
            for task in tasks {
                rows.push(task.await.expect("no scan panics"));
            }
            rows
        })
    });

    assert_eq!(rows.len(), SCANS);
    for read in rows {
        assert_eq!(
            read, ROWS as usize,
            "a scan that gave its worker away still read the whole table"
        );
    }

    assert_eq!(
        gate.pending_answers(),
        SCANS as u64,
        "each scan should have been told once that the bytes were not there, and then woken when \
         they were. More than that is a task being handed back to the scheduler and asking again \
         rather than sleeping on the waker it was invited to leave"
    );
}

#[test]
fn a_host_that_agreed_to_wait_still_gets_its_rows_from_a_source_that_stalls() {
    let bytes = builder(ROWS, COLUMNS)
        .build()
        .expect("the container is writable");
    let runtime = Runtime::new().expect("a runtime starts");

    // One waiter is all this gate is expecting, so it opens the moment the scan parks on it.
    let gate = Arc::new(Gate::new(1));
    let mut windowed = runtime
        .open_windowed(Box::new(stalling(&bytes, &gate)))
        .expect("the container opens");
    gate.arm();

    let read = within(
        "a scan on a thread that agreed to wait never came back, which means the gate it parked on \
         was never told anybody was waiting",
        move || {
            let batches = windowed.scan().expect("the scan runs");
            column_values(&batches, 0).len()
        },
    );
    assert_eq!(read, ROWS as usize);

    // The synchronous path hands the source a waker that does nothing, because the thread calling it
    // is the scheduler and will come back on its own. The source is entitled to take that waker and
    // fire it into nowhere, which is exactly what happens here, and the scan still finishes.
    assert_eq!(gate.pending_answers(), 1);
}
