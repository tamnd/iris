//! WebAssembly execution layer for iris decoders.
//!
//! This is the only crate in the tree that knows Wasmtime exists. Everything above it deals in
//! records and buffers, which is what makes a Wasmtime major version bump a change to one crate
//! rather than a change to the project.
//!
//! # What it does
//!
//! [`Vm`] compiles, [`Program`] is the compiled module, and [`Decoder`] is one instance of it having
//! a conversation with the host. The conversation is four calls out and two calls in, and all six
//! are written down in `docs/ABI.md`:
//!
//! | Direction | Call | What it is for |
//! | --- | --- | --- |
//! | Out | `iris_source(len) -> u32` | Ask the guest for room, then copy the source in |
//! | Out | `iris_input(len) -> u32` | Ask the guest for room, then copy one record in |
//! | Out | `iris_start() -> u64` | Send a `Hello`, get a `HelloAck` or a `Refusal` |
//! | Out | `iris_scan() -> u64` | Send a `ScanRequest`, get nothing or a `Refusal` |
//! | In | `iris.emit(ptr, len) -> u32` | One batch, as it is produced |
//! | In | `iris.require_range(at, len, dst) -> u32` | Bytes the decoder does not have yet |
//!
//! # Pulling, and stopping
//!
//! `require_range` is the call that lets a decoder read a file it cannot hold. The decoder names the
//! bytes it wants and a buffer of its own to put them in, and everything about how they are obtained
//! stays on the host side of the boundary. What makes that workable rather than merely possible is
//! that the host is allowed to answer later: if the source it attached with [`Decoder::attach`] does
//! not have the bytes yet, the decoder is stopped where it stands and the host thread is handed
//! back.
//!
//! That is why [`Decoder::start`] and [`Decoder::scan`] hand back a [`Running`] instead of an answer.
//! [`Running::poll`] moves the call as far as it will go and returns either the answer or
//! [`Progress::Suspended`], and nothing is lost in between: the guest's stack and every row it has
//! already decoded are still there, so a scan that misses on ten thousand ranges suspends ten
//! thousand times rather than starting again ten thousand times. A host with a thread to spare and
//! no interest in any of this calls [`Running::wait`] and gets the old shape back in one word.
//!
//! # The deadline
//!
//! Every call into a decoder is metered, and it is metered whether or not anybody asked. A decoder
//! that loops forever costs the query it was running and nothing else: the call comes back as
//! [`Error::Deadline`] naming the decoder and the budget it was given, and the host thread that made
//! the call is the one that gets control back. There is no switch that turns this off, because a
//! host that can forget is a host one bad decoder away from a wedged thread.
//!
//! [`Vm::with_deadline`] moves the budget, which is the only knob. The default is ten seconds, which
//! no honest decoder reading a resident buffer will ever notice.
//!
//! # What it does not do
//!
//! It does not know what a schema is, what Arrow is, or what the bytes in a buffer mean. A
//! [`RawBatch`] is a row count, a list of nodes and a list of buffers, and whether those describe a
//! valid array is a question for `iris-guard` and for `iris-runtime` above it.
//!
//! It does not meter memory or fuel. A decoder that allocates until the engine says no gets an
//! ordinary trap, and one that spends its whole budget on every call and returns is slow rather than
//! hostile. Both are worth bounding and neither wedges a host, which is why the deadline came first.
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
mod run;

pub use batch::RawBatch;
pub use error::{Error, Result};
pub use instance::{Decoder, Handshake};
pub use module::{Program, Vm};
pub use run::{Progress, Running};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
