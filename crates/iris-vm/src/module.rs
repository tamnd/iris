//! Compiling a decoder, and the engine that does it.

use std::thread;
use std::time::Duration;

use wasmtime::{Config, Engine, Module};

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

    /// Compiles a decoder module.
    ///
    /// The name is what this crate calls the module when something goes wrong, and iris passes the
    /// decoder's digest. That is the only identity a decoder has that means anything: a decoder
    /// that traps or runs away is a specific set of bytes somebody has to go and look at, and its
    /// name for itself is whatever it chose to call itself.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the bytes are not a module this build can compile.
    pub fn compile(&self, wasm: &[u8], decoder: &str) -> Result<Program> {
        let module =
            Module::new(&self.engine, wasm).map_err(|err| Error::Compile(err.to_string()))?;
        Ok(Program {
            engine: self.engine.clone(),
            module,
            decoder: decoder.to_owned(),
            deadline: self.deadline,
        })
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
