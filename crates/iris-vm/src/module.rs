//! Compiling a decoder, and the engine that does it.

use wasmtime::{Config, Engine, Module};

use crate::error::{Error, Result};

/// A compiler and the settings it runs under.
///
/// One of these is meant to be shared. Compiling a module is much more expensive than instantiating
/// one, and an engine caches what it can across both, so a host that makes a fresh engine per scan
/// is paying for the compiler over and over.
#[derive(Clone, Debug)]
pub struct Vm {
    engine: Engine,
}

impl Vm {
    /// An engine with the settings a decoder runs under.
    ///
    /// The settings are deliberately dull. A decoder is a pure function over bytes, so nothing that
    /// would let it reach the outside world is on, and nothing that is still moving in the
    /// specification is on either.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the configuration is not one this build of Wasmtime supports.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_bulk_memory(true)
            .wasm_simd(true)
            .wasm_multi_memory(false)
            .cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = Engine::new(&config).map_err(|err| Error::Compile(err.to_string()))?;
        Ok(Self { engine })
    }

    /// Compiles a decoder module.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compile`] if the bytes are not a module this build can compile.
    pub fn compile(&self, wasm: &[u8]) -> Result<Program> {
        let module =
            Module::new(&self.engine, wasm).map_err(|err| Error::Compile(err.to_string()))?;
        Ok(Program {
            engine: self.engine.clone(),
            module,
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
}

impl Program {
    pub(crate) const fn engine(&self) -> &Engine {
        &self.engine
    }

    pub(crate) const fn module(&self) -> &Module {
        &self.module
    }
}
