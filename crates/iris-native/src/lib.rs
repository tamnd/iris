//! Native implementations of decoders this host already knows, keyed by content hash.
//!
//! A decoder ships inside the dataset and runs in a sandbox, which is the whole arrangement and is
//! what makes a container from anywhere readable here. It also costs something: the WebAssembly
//! vector width is capped at 128 bits and will be for a while yet, so a decoder that would use the
//! full width of the machine cannot. A host that has its own implementation of a decoder it
//! recognises should be able to run that instead and skip the sandbox.
//!
//! ```no_run
//! use std::sync::Arc;
//! use iris_native::Registry;
//! # use iris_abi::{Hello, ScanRequest};
//! # use iris_source::RangeSource;
//! # use iris_vm::Handshake;
//! # #[derive(Debug)]
//! # struct Mine;
//! # impl iris_native::Native for Mine {
//! #     fn handshake(&self, _: &Hello) -> iris_native::Result<Handshake> { unimplemented!() }
//! #     fn scan<'a>(&'a self, _: &'a ScanRequest<'a>, _: &'a mut (dyn RangeSource + Send))
//! #         -> iris_native::Scanning<'a> { unimplemented!() }
//! # }
//! let module = std::fs::read("fixedwidth.wasm")?;
//! let registry = Registry::new().with_module(&module, Arc::new(Mine));
//!
//! // A container carrying exactly those bytes now runs `Mine`. A container carrying any other
//! // bytes runs in the sandbox, whatever it calls its decoder.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Recognising is the hard part
//!
//! The obvious way to build this is a table from decoder name to implementation, and the obvious way
//! is unsound. The name in a container is chosen by whoever wrote the container. A dataset that
//! calls its decoder `fixedwidth` gets this host's idea of what `fixedwidth` means, which is native
//! code running with no sandbox around it against bytes laid out by somebody who only had to type a
//! string to get there. That is not a subtle failure. It is arbitrary code selection by filename.
//!
//! So the key here is the digest of the decoder module and there is nothing else in it. The digest
//! is not what the container claims either, it is what `iris-trust` computed from the module bytes
//! that were actually present, which is the same value that would have been compiled had the
//! sandbox run. A decoder whose digest is not in the table gets the sandbox no matter what it calls
//! itself, and that is a property of the shape of [`Registry`] rather than of anybody remembering to
//! check.
//!
//! # What substitution does not skip
//!
//! Everything except running the module. The module is still read out of the container and still
//! hashed and checked, since the digest is what selects the implementation. The handshake still
//! happens and is still negotiated by the same function. Every batch still goes through `iris-guard`
//! and is still validated by Arrow. What is skipped is compiling and instantiating, and nothing
//! else, so a native implementation with a bug in it is caught by the same checks that catch a
//! guest with a bug in it.
//!
//! # What is not here yet
//!
//! No implementation of anything. This crate is the table and the trait, and the kernels that go in
//! it come with the differential runs that prove they agree with the modules they stand in for. A
//! native kernel without one of those is worth less than nothing, because it is a second decoder
//! that quietly disagrees with the first.

#![forbid(unsafe_code)]

mod native;
mod registry;

pub use native::{Error, Native, Result, Scanning};
pub use registry::Registry;

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
