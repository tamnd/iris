//! Compiling a decoder, and the engine that does it.

use std::hash::{Hash as _, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use iris_format::Digest;
use wasmtime::{Config, Engine, Module};

use crate::cache::Cache;
use crate::error::{Error, Result};

/// How often the epoch counter moves.
///
/// This is the resolution of every deadline in this crate, and it is a cost paid whether or not
/// anything is running: one thread wakes up this often for as long as an engine exists. Ten
/// milliseconds is fine grained enough that a deadline of a second means roughly a second, and
/// coarse enough that the thread costs nothing measurable.
const TICK: Duration = Duration::from_millis(10);

/// How long one call into a decoder may take before it is stopped.
///
/// A scan that reads eight thousand rows out of a resident buffer is milliseconds of work, so ten
/// seconds is not a budget any honest decoder notices. It is there for the decoder that never
/// returns, and the number that matters about it is that it is finite.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// A compiler and the settings it runs under.
///
/// One of these is meant to be shared. Compiling a module is much more expensive than instantiating
/// one, and an engine caches what it can across both, so a host that makes a fresh engine per scan
/// is paying for the compiler over and over.
#[derive(Clone, Debug)]
pub struct Vm {
    engine: Engine,
    deadline: Duration,
    /// Where compiled decoders are kept between processes, if anywhere.
    ///
    /// Behind a handle so that a clone of this shares the directory and the counters with the
    /// original. A clone is the ordinary way an engine gets from the thread that configured it to the
    /// threads that use it, and a clone that kept its own tally would be a tally nobody can read.
    cache: Option<Arc<Cache>>,
}

impl Vm {
    /// An engine with the settings a decoder runs under.
    ///
    /// The settings are deliberately dull. A decoder is a pure function over bytes, so nothing that
    /// would let it reach the outside world is on, and nothing that is still moving in the
    /// specification is on either.
    ///
    /// Epoch metering is on, and it is on here rather than being something a host switches on,
    /// because a host that forgets is a host one bad decoder away from a wedged thread. A thread is
    /// started alongside the engine to move the epoch counter, and it stops when the last handle to
    /// the engine goes away.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the configuration is not one this build of Wasmtime supports,
    /// and [`Error::Metering`] if the thread that meters decoders cannot be started. The second one
    /// refuses to hand back an engine rather than hand back an unmetered one.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_bulk_memory(true)
            .wasm_simd(true)
            .wasm_multi_memory(false)
            .epoch_interruption(true)
            .cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = Engine::new(&config).map_err(|err| Error::Compile(err.to_string()))?;
        start_ticking(&engine)?;
        Ok(Self {
            engine,
            deadline: DEFAULT_DEADLINE,
            cache: None,
        })
    }

    /// Sets how long one call into a decoder may take.
    ///
    /// The budget is per call rather than per scan, because a call is the unit the host gets
    /// control back at. A deadline shorter than one tick is rounded up to one, since there is no
    /// way to notice anything faster than the counter moves.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// How long one call into a decoder compiled here may take.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Keeps compiled decoders in a directory, so that the next process does not compile them again.
    ///
    /// Off by default. Compiling is most of what opening a container costs, and the result is the
    /// same every time, so a host that opens the same handful of decoders over and over is doing the
    /// same work on every start. Naming a directory here turns the second start into a read.
    ///
    /// # What may be in the directory
    ///
    /// Only what this ran. An entry is machine code and loading one maps it executable, so a
    /// directory somebody else can write into is a directory that can hand this process anything.
    /// That is the same rule the decoder resolver follows and it is the operator's to keep: point
    /// this at storage the host owns.
    ///
    /// What cannot go wrong is an entry from a different compiler. The key covers the module and
    /// everything about this engine that decides what compiling it produces, so an upgrade of
    /// Wasmtime, a change of target, or a change of settings misses every entry that was there and
    /// fills the directory again rather than loading code built for something else.
    ///
    /// # What it costs when it fails
    ///
    /// Nothing but the compile that would have happened anyway. A directory that cannot be created, a
    /// file that cannot be written, an entry that will not load: each of those falls through to
    /// compiling the ordinary way. A cache that can fail an open is worse than no cache.
    #[must_use]
    pub fn with_compilation_cache(mut self, dir: impl Into<PathBuf>) -> Self {
        let fingerprint = self.fingerprint();
        self.cache = Some(Arc::new(Cache::new(dir.into(), fingerprint)));
        self
    }

    /// How many decoders were loaded out of the compilation cache rather than compiled.
    ///
    /// Zero when there is no cache, and zero on the first run against an empty directory. A host
    /// where this stays zero across restarts has a directory it is not managing to write to, which is
    /// the failure this reports because nothing else does: every way the cache can fail is silent by
    /// design.
    #[must_use]
    pub fn compilations_reused(&self) -> u64 {
        self.cache.as_ref().map_or(0, |cache| cache.reused())
    }

    /// How many compiled decoders were written into the compilation cache.
    ///
    /// Together with [`Vm::compilations_reused`] this is the whole picture: stored counts the work
    /// this run did for the next one, reused counts the work an earlier run did for this one.
    #[must_use]
    pub fn compilations_stored(&self) -> u64 {
        self.cache.as_ref().map_or(0, |cache| cache.stored())
    }

    /// Compiles a decoder module.
    ///
    /// The name is what this crate calls the module when something goes wrong, and iris passes the
    /// decoder's digest. That is the only identity a decoder has that means anything: a decoder
    /// that traps or runs away is a specific set of bytes somebody has to go and look at, and its
    /// name for itself is whatever it chose to call itself.
    ///
    /// If [`Vm::with_compilation_cache`] named a directory, this looks there first and puts what it
    /// compiles there afterwards. Everything that can go wrong with that ends here, doing what this
    /// would have done anyway, so a caller has nothing to handle differently.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the bytes are not a module this build can compile.
    pub fn compile(&self, wasm: &[u8], decoder: &str) -> Result<Program> {
        match &self.cache {
            Some(cache) => cache.compile(self, wasm, decoder),
            None => self.compile_directly(wasm, decoder),
        }
    }

    /// Compiles without looking in the cache or writing to it.
    ///
    /// This is what the cache falls back to, so it must not be what the cache is reached through.
    pub(crate) fn compile_directly(&self, wasm: &[u8], decoder: &str) -> Result<Program> {
        let module =
            Module::new(&self.engine, wasm).map_err(|err| Error::Compile(err.to_string()))?;
        Ok(self.wrap(module, decoder))
    }

    /// Everything about this engine that decides what compiling produces.
    ///
    /// Two engines that answer the same way here compile the same module to the same machine code,
    /// and an artefact from one loads into the other. It covers the target triple, the compiler
    /// flags, the flags of the instruction set the compiler is targeting, the tunables, the
    /// WebAssembly features that are on, and the version of Wasmtime, because all of those change
    /// what comes out and any of them changing has to be a different answer.
    ///
    /// It is worth being clear about what this is not. It is not a checksum of an artefact and it
    /// says nothing about whether a particular sequence of bytes is one. It is the identity of the
    /// compiler, which is the half of a compilation cache key that is not the module.
    ///
    /// The components come from Wasmtime rather than from a list written here. Wasmtime is the one
    /// that knows which of its settings reach the compiler, and a list maintained on this side would
    /// be a list that is quietly wrong for one release after every upgrade. What this adds is width:
    /// the components are collected rather than mixed down to a machine word, and hashed once at the
    /// end, so two different compilers colliding is not something a cache has to think about.
    #[must_use]
    pub fn fingerprint(&self) -> Digest {
        let mut collected = Collecting(Vec::new());
        self.engine
            .precompile_compatibility_hash()
            .hash(&mut collected);
        Digest::of(&collected.0)
    }

    /// Compiles a decoder to machine code that can be kept and loaded later.
    ///
    /// This does the same work [`Vm::compile`] does and hands back the result instead of a module
    /// ready to run, so that a host with somewhere to put it does not have to do the work again next
    /// time. What comes back is only loadable by an engine that answers [`Vm::fingerprint`] the way
    /// this one does, and [`Vm::load`] is where that stops being an assumption.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the bytes are not a module this build can compile.
    pub(crate) fn precompile(&self, wasm: &[u8]) -> Result<Vec<u8>> {
        self.engine
            .precompile_module(wasm)
            .map_err(|err| Error::Compile(err.to_string()))
    }

    /// Loads machine code this engine produced earlier.
    ///
    /// # What a caller is promising
    ///
    /// That the bytes are the unmodified output of [`Vm::precompile`] from an engine with this
    /// engine's [`fingerprint`](Vm::fingerprint). They are machine code and they are about to be
    /// mapped executable, so bytes that are neither of those things are not a parse error, they are
    /// whatever the machine does with them.
    ///
    /// This is not a hole in the sandbox and it is worth saying why not. A decoder in a container is
    /// still compiled from the module the container carried, and the digest of that module is still
    /// checked before anything happens to it. What a caller may hand to this is an artefact its own
    /// earlier compile produced, out of storage it controls, and a host that lets somebody else
    /// write into that storage has already lost. That is the same shape as the decoder resolver: off
    /// by default, and an operator's decision when it is on.
    ///
    /// Wasmtime does check the marker it wrote into the artefact and refuses one from a different
    /// version of itself, which turns the ordinary mistake into an error. It is a courtesy rather
    /// than a guarantee, because a courtesy is all a check inside the bytes being checked can be.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if Wasmtime will not accept the artefact, which is what happens to
    /// one written by a different version or a differently configured engine.
    ///
    /// # Safety
    ///
    /// The bytes must be the unaltered output of [`Vm::precompile`] on an engine whose
    /// [`fingerprint`](Vm::fingerprint) equals this one's.
    #[allow(unsafe_code, reason = "the compilation cache, see the note in lib.rs")]
    pub(crate) unsafe fn load(&self, artefact: &[u8], decoder: &str) -> Result<Program> {
        // SAFETY: the caller promises the bytes are an artefact this engine produced, which is the
        // whole of what this function's own contract asks of it and is passed straight through.
        let module = unsafe { Module::deserialize(&self.engine, artefact) }
            .map_err(|err| Error::Compile(err.to_string()))?;
        Ok(self.wrap(module, decoder))
    }

    /// The bookkeeping both ways of getting a module share.
    fn wrap(&self, module: Module, decoder: &str) -> Program {
        Program {
            engine: self.engine.clone(),
            module,
            decoder: decoder.to_owned(),
            deadline: self.deadline,
        }
    }
}

/// A [`Hasher`] that keeps what it was given instead of mixing it.
///
/// [`Vm::fingerprint`] needs everything Wasmtime hashes into its compatibility value, and the trait
/// that reaches it hands out sixty four bits. So this stands in for a hasher, collects the bytes,
/// and lets a real hash run over them once at the end. `finish` is never the answer here and returns
/// zero, which is honest: nothing asks this for a hash.
///
/// The lengths of the variable length pieces go in as well, so that two different sequences of
/// fields cannot be collected into the same run of bytes by moving a boundary.
struct Collecting(Vec<u8>);

impl Hasher for Collecting {
    fn write(&mut self, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).expect("a field of a compiler's settings is not huge");
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        0
    }
}

/// A compiled decoder, ready to be instantiated.
///
/// Compiling once and instantiating many times is the whole reason this is a separate type. A scan
/// that reads a hundred containers written by the same decoder compiles it once.
#[derive(Clone, Debug)]
pub struct Program {
    engine: Engine,
    module: Module,
    decoder: String,
    deadline: Duration,
}

impl Program {
    /// What the host calls this decoder, which for iris is its digest.
    #[must_use]
    pub fn decoder(&self) -> &str {
        &self.decoder
    }

    /// How long one call into this decoder may take.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The same compiled module, with a different budget for a call into it.
    ///
    /// Compiling and metering are separate questions and this is what keeps them separate. The
    /// deadline is a property of the host's patience rather than of the code, so a host that keeps
    /// compiled modules around and shares them between callers who disagree about how long a call
    /// may take restamps one rather than compiling the same bytes twice. Nothing about the module is
    /// touched, and the engine handle is the same handle, so this costs a clone of the decoder's
    /// name.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub(crate) const fn engine(&self) -> &Engine {
        &self.engine
    }

    pub(crate) const fn module(&self) -> &Module {
        &self.module
    }

    /// The deadline in epoch ticks, which is the unit a store counts in.
    pub(crate) fn ticks(&self) -> u64 {
        let ticks = self.deadline.as_millis() / TICK.as_millis();
        u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
    }
}

/// Starts the thread that moves the epoch counter for an engine.
///
/// It holds a weak handle rather than the engine, so the thread ends when the last real handle is
/// dropped. Holding a strong one would keep every engine ever built alive for the life of the
/// process, which is a leak in the shape of a metering feature.
fn start_ticking(engine: &Engine) -> Result<()> {
    let weak = engine.weak();
    thread::Builder::new()
        .name("iris-epoch".to_owned())
        .spawn(move || {
            loop {
                thread::sleep(TICK);
                let Some(engine) = weak.upgrade() else {
                    return;
                };
                engine.increment_epoch();
            }
        })
        .map_err(|err| Error::Metering(err.to_string()))?;
    Ok(())
}
