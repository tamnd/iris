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
//!
//! use iris_abi::{Capability, CapabilitySet};
//! use iris_native::{Case, Corpus, Differential, Registry};
//! use iris_vm::Vm;
//! # use iris_abi::{Hello, ScanRequest};
//! # use iris_source::RangeSource;
//! # use iris_vm::Handshake;
//! # #[derive(Debug)]
//! # struct Mine;
//! # impl iris_native::Native for Mine {
//! #     fn handshake(&self, _: &Hello) -> iris_native::Result<Handshake> { unimplemented!() }
//! #     fn scan<'a>(&'a self, _: &'a Hello, _: &'a ScanRequest<'a>,
//! #         _: &'a mut (dyn RangeSource + Send)) -> iris_native::Scanning<'a> { unimplemented!() }
//! # }
//! let vm = Vm::new()?;
//! let module = std::fs::read("fixedwidth.wasm")?;
//!
//! // The datasets the two implementations have to agree about, byte for byte.
//! let corpus = Corpus::new().with_case(Case::new(
//!     "readings",
//!     std::fs::read("readings.bin")?,
//!     1_000,
//!     3,
//! ));
//!
//! let terms = CapabilitySet::new()
//!     .with(Capability::RANDOM_ACCESS)
//!     .with(Capability::PROJECTION);
//! let kernel = Differential::new(&vm, &module)
//!     .offering(terms)
//!     .verify(Arc::new(Mine), &corpus)?;
//! let registry = Registry::new().with(kernel);
//!
//! // A container carrying exactly those module bytes now runs `Mine`. A container carrying any
//! // other bytes runs in the sandbox, whatever it calls its decoder.
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
//! # Recognising is not enough on its own
//!
//! Knowing which module a kernel stands in for says nothing about whether it stands in for it
//! correctly. A native implementation that disagrees with the module is a second decoder for the
//! same bytes, and the one that runs is the one nobody read, so a wrong answer arrives as data
//! rather than as an error.
//!
//! [`Differential`] is what closes that, and it closes it in the type system rather than in a
//! checklist. It runs both implementations over a [`Corpus`] and compares what comes back down to
//! the byte, and what it hands out on success is a [`Kernel`]. [`Registry::with`] takes a [`Kernel`]
//! and there is no other way to make one, so a host that registers an implementation without
//! running the comparison does not compile. That is worth more than a build script or a test, both
//! of which can be skipped by somebody in a hurry.
//!
//! What the run covers is the corpus it was given, and [`Corpus`] is plain about what that is worth.
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
//! No implementation of anything. This crate is the table, the trait and the harness that proves a
//! kernel belongs in the table. The kernels themselves come later, and each one arrives with the
//! corpus it was proved against.

#![forbid(unsafe_code)]

mod corpus;
mod differential;
mod native;
mod registry;

pub use corpus::{Case, Corpus};
pub use differential::{Differential, Kernel, Mismatch};
pub use native::{Error, Native, Result, Scanning};
pub use registry::Registry;

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
