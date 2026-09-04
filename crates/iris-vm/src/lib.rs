//! WebAssembly execution layer for iris decoders.
//!
//! This is the only crate in the tree that knows Wasmtime exists. Everything above it deals in
//! records and buffers, which is what makes a Wasmtime major version bump a change to one crate
//! rather than a change to the project.
//!
//! # What it does
//!
//! [`Vm`] compiles, [`Program`] is the compiled module, and [`Decoder`] is one instance of it having
//! a conversation with the host. The conversation is four calls out and one call in, and all five
//! are written down in `docs/ABI.md`:
//!
//! | Direction | Call | What it is for |
//! | --- | --- | --- |
//! | Out | `iris_source(len) -> u32` | Ask the guest for room, then copy the source in |
//! | Out | `iris_input(len) -> u32` | Ask the guest for room, then copy one record in |
//! | Out | `iris_start() -> u64` | Send a `Hello`, get a `HelloAck` or a `Refusal` |
//! | Out | `iris_scan() -> u64` | Send a `ScanRequest`, get nothing or a `Refusal` |
//! | In | `iris.emit(ptr, len) -> u32` | One batch, as it is produced |
//!
//! # What it does not do
//!
//! It does not know what a schema is, what Arrow is, or what the bytes in a buffer mean. A
//! [`RawBatch`] is a row count, a list of nodes and a list of buffers, and whether those describe a
//! valid array is a question for `iris-guard` at M2 and for `iris-runtime` above it.
//!
//! It does not meter anything yet. A decoder that loops forever loops forever, which is fine while
//! every decoder in the tree is one somebody in this repository wrote and is not fine the moment one
//! is not. Epoch metering is M2 and it is a gate rather than a nice to have.
//!
//! # The copy
//!
//! Every buffer a batch points at is copied out of the guest while the guest is still stopped inside
//! the `emit` call that produced it. That is not caution, it is correctness: a decoder is allowed to
//! reuse its buffers between batches, so the bytes at an offset are only certainly the batch's bytes
//! during that call. Taking them later would be reading whatever the next batch happened to write.

#![forbid(unsafe_code)]

mod batch;
mod error;
mod instance;
mod module;

pub use batch::RawBatch;
pub use error::{Error, Result};
pub use instance::{Decoder, Handshake};
pub use module::{Program, Vm};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
