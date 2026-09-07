//! A bounded store of things that were expensive to build, shared by every scan.
//!
//! Compiling a decoder is the most expensive thing this crate does, and the arrangement a query
//! engine puts it in is the worst one for it. A DataFusion query over an iris table opens the
//! container once per output partition, and each open compiles the same decoder again, so a scan
//! split four ways pays for the compiler four times to run four copies of identical code. The
//! module is identified by the hash of its bytes and the hash was checked before anything compiled
//! it, so two opens that name the same digest are asking for the same artifact and there is no
//! question of them differing.
//!
//! # Why it is a pool and not a map
//!
//! Compiled code is memory, and a host that keeps every decoder it has ever seen has a leak with a
//! cache in front of it. So this holds a budget, replaces what has gone longest without being asked
//! for when a new entry does not fit, and refuses to hold anything larger than the whole budget
//! rather than emptying itself for one entry.
//!
//! Evicting is dropping this side's handle and nothing more. Whoever asked for an entry was handed a
//! clone, so an entry that is evicted while a scan is running is still perfectly alive for that
//! scan, and the memory goes back when the last holder is finished with it. That is what makes
//! eviction safe to do at any moment rather than something that has to wait for readers.
//!
//! # The two paths that are worth modelling
//!
//! The handoff. Two threads that want the same key at the same time must produce one build and not
//! two, which means the second one waits for the first rather than starting its own. Getting that
//! wrong is not a crash and not a wrong answer, it is the cache silently doing nothing on exactly
//! the workload it was built for, which is a bug no assertion about results would ever see.
//!
//! The eviction. A build happens with the lock dropped, because holding a lock across a compiler is
//! how one slow decoder becomes every query's problem, so the state a build comes back to is not the
//! state it left. Two builds can finish at once, into a budget that has room for one.
//!
//! Neither of those is a bug an iteration count finds. A stress run samples interleavings and these
//! are the ones it does not sample, so the models under `cfg(loom)` at the bottom of this file
//! enumerate them instead. They are the M6 loom gate, and they run in the nightly job with
//! `RUSTFLAGS=--cfg loom`, which is a configuration flag and deliberately not a cargo feature: a
//! feature can be turned on by a dependency resolving somewhere else in the graph, and loom's
//! primitives in a release build would be a disaster nobody asked for.

#[cfg(loom)]
use loom::sync::{Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex, MutexGuard};

/// Things that were expensive to build, kept while there is room for them.
///
/// Generic over what it holds so that the loom models can drive it with an integer. A model runs the
/// same code many thousands of times, once per interleaving, and one that compiled a real module
/// each time would be measuring Cranelift rather than exploring orderings.
#[derive(Debug)]
pub(crate) struct Pool<K, V> {
    state: Mutex<State<K, V>>,
    /// Woken whenever a key stops being built, whether it was built or not.
    ready: Condvar,
    budget: usize,
}

/// Everything one lock covers.
///
/// The entries, what they weigh and who is building what are all in here together rather than in a
/// lock each, because every interesting operation touches more than one of them. Splitting them
/// would buy nothing on a structure that is held for a handful of comparisons at a time, and it
/// would make the budget a number two threads can disagree about.
#[derive(Debug)]
struct State<K, V> {
    /// Most recently asked for first, which is also the replacement order read backwards.
    ///
    /// A list rather than a map because a host has a handful of decoders and not thousands of them,
    /// so a linear scan of it is shorter than hashing a key would be.
    entries: Vec<Entry<K, V>>,
    /// The keys somebody is building right now.
    building: Vec<K>,
    /// What the entries weigh, kept alongside them so that admitting one is not a walk.
    held: usize,
    /// How many builds this pool has started, which is the number that says whether it is working.
    builds: u64,
}

/// One thing held, and what it costs to hold it.
#[derive(Debug)]
struct Entry<K, V> {
    key: K,
    value: V,
    weight: usize,
}

impl<K: Clone + Eq, V: Clone> Pool<K, V> {
    /// A pool that will hold `budget` bytes of whatever it is weighed in.
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            state: Mutex::new(State {
                entries: Vec::new(),
                building: Vec::new(),
                held: 0,
                builds: 0,
            }),
            ready: Condvar::new(),
            budget,
        }
    }

    /// Hands back what is held under `key`, building it first if nobody else already is.
    ///
    /// A caller that arrives while another thread is building the same key waits for it and takes
    /// what that thread produced. A caller that arrives while another thread is building a different
    /// key does not wait at all, because the lock is not held across a build.
    ///
    /// The value comes back whether or not it was kept. An entry heavier than the whole budget is
    /// built, handed over and not held, which is the case a caller should never notice: this is a
    /// cache and the answer does not depend on whether it hit.
    ///
    /// # Errors
    ///
    /// Whatever `build` returns. A failed build is not remembered, so the next caller tries again,
    /// and a build that panics leaves the pool exactly as available as a build that returned an
    /// error.
    pub(crate) fn get_or_build<E>(
        &self,
        key: &K,
        weight: usize,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        let mut state = self.lock();
        loop {
            if let Some(value) = Self::take(&mut state, key) {
                return Ok(value);
            }
            // Waited on in a loop and not once, which is the ordinary condition variable discipline
            // and is doing more work here than usual. A waiter can be woken because its own key
            // finished, because somebody else's did, or for no reason at all, and the third of those
            // is allowed by the platform rather than by anything in this file.
            if !state.building.iter().any(|building| building == key) {
                break;
            }
            state = self.wait(state);
        }

        state.building.push(key.clone());
        state.builds += 1;
        drop(state);

        // From here to the end of the claim's life this thread owes every waiter an answer, and the
        // claim is what pays that debt however this block ends. A build that returns an error and a
        // build that panics both have to release the key and wake whoever is waiting on it, and a
        // panic has no other way of doing that.
        let claim = Claim {
            pool: self,
            key: key.clone(),
        };
        let built = build();
        if let Ok(value) = &built {
            let mut state = self.lock();
            self.admit(&mut state, key.clone(), value.clone(), weight);
        }
        drop(claim);
        built
    }

    /// How many builds this pool has started since it was made.
    ///
    /// The number that says whether sharing is working. Opening the same container from eight
    /// partitions should move this by one.
    pub(crate) fn builds(&self) -> u64 {
        self.lock().builds
    }

    /// How many entries are held.
    pub(crate) fn entries(&self) -> usize {
        self.lock().entries.len()
    }

    /// What the entries held weigh, which never goes above the budget.
    pub(crate) fn held(&self) -> usize {
        self.lock().held
    }

    /// The value under a key, moved to the front because asking for it is using it.
    fn take(state: &mut State<K, V>, key: &K) -> Option<V> {
        let at = state.entries.iter().position(|entry| &entry.key == key)?;
        let entry = state.entries.remove(at);
        let value = entry.value.clone();
        state.entries.insert(0, entry);
        Some(value)
    }

    /// Puts an entry in, throwing out the least recently asked for until it fits.
    ///
    /// An entry that is heavier than the whole budget is not held. Making room for it would mean
    /// emptying the pool for something that then has nothing to share the pool with, and a host that
    /// set a budget smaller than one decoder meant the budget.
    fn admit(&self, state: &mut State<K, V>, key: K, value: V, weight: usize) {
        if weight > self.budget {
            return;
        }
        while state.held + weight > self.budget {
            let Some(evicted) = state.entries.pop() else {
                break;
            };
            state.held -= evicted.weight;
        }

        // Not already here, because the only way in is through a claim and a claim is only taken
        // when a lookup missed. A build that overlapped with another build of the same key is the
        // thing the claim exists to prevent.
        state.held += weight;
        state.entries.insert(0, Entry { key, value, weight });
    }

    /// The state, whether or not a thread panicked while it held it.
    ///
    /// Recovered rather than propagated, which is the right call for a cache and would not be for
    /// much else. Nothing in here is left half written by a panic: the only code that runs under
    /// this lock is a handful of comparisons and moves on a `Vec`, and the expensive step that could
    /// actually panic is deliberately outside it. Refusing to hand back a compiled decoder for the
    /// rest of the process because one unrelated allocation failed would turn a bad moment into a
    /// permanent one.
    fn lock(&self) -> MutexGuard<'_, State<K, V>> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Waits until something finishes being built, for the reason given at [`Pool::lock`].
    fn wait<'p>(&'p self, state: MutexGuard<'p, State<K, V>>) -> MutexGuard<'p, State<K, V>> {
        match self.ready.wait(state) {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// A key this thread has said it is building, released however the build ends.
///
/// It exists for the panic. An error is a value and could have been handled with a match, but a
/// build that unwinds would otherwise leave its key in `building` with nobody left to take it out,
/// and every later caller for that decoder would wait for a thread that is gone. A cache turning a
/// recoverable panic into a permanently wedged host is a much worse failure than the one it started
/// from.
struct Claim<'p, K: Clone + Eq, V: Clone> {
    pool: &'p Pool<K, V>,
    key: K,
}

impl<K: Clone + Eq, V: Clone> Drop for Claim<'_, K, V> {
    fn drop(&mut self) {
        let mut state = self.pool.lock();
        if let Some(at) = state
            .building
            .iter()
            .position(|building| building == &self.key)
        {
            state.building.remove(at);
        }
        drop(state);

        // Every waiter and not one of them. There is one condition variable for the whole pool, so
        // a waiter woken here may well be waiting on a different key, and waking a single arbitrary
        // one would let the wakeup land on a thread that goes straight back to sleep while the
        // thread this was meant for waits for a build that has already finished. A condition
        // variable per key would be the other answer, and it would mean allocating one per decoder
        // to save a handful of threads a re-check of a list with a handful of entries in it.
        self.pool.ready.notify_all();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::Pool;

    /// A pool of strings, weighed in bytes, which is enough to check every path that is not about
    /// two threads.
    fn pool(budget: usize) -> Pool<u32, String> {
        Pool::new(budget)
    }

    /// Builds a value and says so, so a test can tell a hit from a miss.
    fn build(calls: &AtomicU64, value: &str) -> Result<String, ()> {
        calls.fetch_add(1, Ordering::Relaxed);
        Ok(value.to_owned())
    }

    #[test]
    fn the_second_ask_for_a_key_does_not_build_it_again() {
        let pool = pool(1024);
        let calls = AtomicU64::new(0);

        assert_eq!(
            pool.get_or_build(&1, 8, || build(&calls, "one")),
            Ok("one".to_owned())
        );
        assert_eq!(
            pool.get_or_build(&1, 8, || build(&calls, "one")),
            Ok("one".to_owned())
        );

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(pool.builds(), 1);
        assert_eq!(pool.held(), 8);
    }

    #[test]
    fn the_one_that_has_gone_longest_without_being_asked_for_is_the_one_replaced() {
        let pool = pool(20);
        let calls = AtomicU64::new(0);

        for key in 1..=2 {
            let _ = pool.get_or_build(&key, 10, || build(&calls, "held"));
        }
        // Asking for the first again makes the second the oldest, so the third replaces it rather
        // than replacing the one that has just been used.
        let _ = pool.get_or_build(&1, 10, || build(&calls, "held"));
        let _ = pool.get_or_build(&3, 10, || build(&calls, "held"));

        assert_eq!(pool.entries(), 2);
        assert_eq!(pool.held(), 20);
        assert_eq!(calls.load(Ordering::Relaxed), 3, "nothing was rebuilt yet");

        let _ = pool.get_or_build(&2, 10, || build(&calls, "held"));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            4,
            "the second key was the one thrown out, so asking for it builds it again"
        );
    }

    #[test]
    fn something_larger_than_the_whole_budget_is_handed_over_and_not_held() {
        let pool = pool(16);
        let calls = AtomicU64::new(0);

        let _ = pool.get_or_build(&1, 8, || build(&calls, "small"));
        assert_eq!(
            pool.get_or_build(&2, 64, || build(&calls, "large")),
            Ok("large".to_owned())
        );

        assert_eq!(pool.entries(), 1, "the small one was not thrown out for it");
        assert_eq!(pool.held(), 8);
    }

    #[test]
    fn a_build_that_fails_is_not_remembered() {
        let pool = pool(1024);

        assert_eq!(pool.get_or_build(&1, 8, || Err::<String, _>("no")), Err("no"));
        assert_eq!(pool.entries(), 0);

        let calls = AtomicU64::new(0);
        assert_eq!(
            pool.get_or_build(&1, 8, || build(&calls, "one")),
            Ok("one".to_owned()),
            "the next caller tries again rather than being handed the failure"
        );
    }

    #[test]
    fn a_build_that_panics_leaves_the_key_available() {
        let pool = pool(1024);

        // Asserted rather than derived, because a `Pool` holds a mutex and so is unwind safe in the
        // sense the trait means, and the thing actually being checked here is what the pool looks
        // like afterwards rather than whether anything was left half written.
        let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            pool.get_or_build(&1, 8, || -> Result<String, ()> {
                panic!("the compiler fell over")
            })
        }));
        assert!(panicked.is_err());

        let calls = AtomicU64::new(0);
        assert_eq!(
            pool.get_or_build(&1, 8, || build(&calls, "one")),
            Ok("one".to_owned()),
            "a key whose build panicked is not a key nobody can ever build again"
        );
    }
}

/// The interleavings a stress run does not reach.
///
/// Built and run only under `RUSTFLAGS=--cfg loom`, which is what the M6 gate asks for: a
/// configuration flag rather than a cargo feature, so that no dependency resolution anywhere can
/// turn loom's primitives on in something somebody ships.
///
/// Every model here builds an integer rather than a decoder. loom runs the closure once per
/// interleaving and there are thousands of them, so a model that compiled a real module would be an
/// experiment about Cranelift's throughput.
#[cfg(all(test, loom))]
mod loom_model {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU64, Ordering};

    use super::Pool;

    /// Room for two entries of the weight the models use.
    const ROOMY: usize = 16;

    /// The handoff: one build, however the two threads are ordered.
    ///
    /// Both threads want the same key. Whichever gets there first builds it, and the other one
    /// either waits for that build and takes what it produced or arrives after it finished and finds
    /// it held. There is no ordering where both compile, and there is no ordering where either of
    /// them comes away without a value.
    #[test]
    fn loom_two_threads_wanting_one_key_build_it_once() {
        loom::model(|| {
            let pool = Arc::new(Pool::<u32, u64>::new(ROOMY));
            let built = Arc::new(AtomicU64::new(0));

            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let pool = Arc::clone(&pool);
                    let built = Arc::clone(&built);
                    loom::thread::spawn(move || {
                        pool.get_or_build(&7, 8, || {
                            built.fetch_add(1, Ordering::Relaxed);
                            Ok::<u64, ()>(70)
                        })
                    })
                })
                .collect();

            for thread in threads {
                let got = thread.join().expect("no model here panics");
                assert_eq!(got, Ok(70), "a waiter is handed what the builder built");
            }
            assert_eq!(
                built.load(Ordering::Relaxed),
                1,
                "the second thread waited for the first rather than compiling it again"
            );
        });
    }

    /// A build that fails does not leave the other thread waiting for it.
    ///
    /// The first thread claims the key and then fails. The second either has already started waiting
    /// and has to be woken by a build that produced nothing, or arrives afterwards and finds neither
    /// an entry nor a claim. Both orderings have to end with it building the value itself, and the
    /// one that matters is the first, because that is the wakeup a failed build has no obvious
    /// reason to send.
    #[test]
    fn loom_a_failed_build_hands_the_key_to_whoever_is_waiting() {
        loom::model(|| {
            let pool = Arc::new(Pool::<u32, u64>::new(ROOMY));

            let failing = {
                let pool = Arc::clone(&pool);
                loom::thread::spawn(move || pool.get_or_build(&7, 8, || Err::<u64, u32>(1)))
            };
            let waiting = {
                let pool = Arc::clone(&pool);
                loom::thread::spawn(move || pool.get_or_build(&7, 8, || Ok::<u64, u32>(70)))
            };

            let first = failing.join().expect("no model here panics");
            let second = waiting.join().expect("no model here panics");
            assert_eq!(first, Err(1));
            assert_eq!(
                second,
                Ok(70),
                "the thread that could build it was not left waiting on the one that could not"
            );
        });
    }

    /// Two builds finishing at once into a budget with room for one.
    ///
    /// Both threads build, because the keys are different, and both come back to a pool whose state
    /// is not the state they left. Whichever admits second evicts the first. What has to hold at the
    /// end is that the budget was not exceeded and that the accounting matches the entries, in every
    /// ordering, since the two admissions are the only place a weight is added and the eviction that
    /// pays for one of them happens in between.
    #[test]
    fn loom_two_builds_landing_at_once_do_not_overrun_the_budget() {
        loom::model(|| {
            // Room for one of the two, so one admission has to evict the other.
            let pool = Arc::new(Pool::<u32, u64>::new(8));

            let threads: Vec<_> = (1..=2)
                .map(|key| {
                    let pool = Arc::clone(&pool);
                    loom::thread::spawn(move || {
                        pool.get_or_build(&key, 8, || Ok::<u64, ()>(u64::from(key)))
                    })
                })
                .collect();

            for (at, thread) in threads.into_iter().enumerate() {
                let got = thread.join().expect("no model here panics");
                assert_eq!(
                    got,
                    Ok(at as u64 + 1),
                    "an entry evicted by the other thread is still the value this one built"
                );
            }

            assert_eq!(pool.entries(), 1, "a budget with room for one holds one");
            assert_eq!(pool.held(), 8, "and what it says it holds is what it holds");
        });
    }
}
