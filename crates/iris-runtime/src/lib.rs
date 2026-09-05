//! The iris runtime an engine embeds.
//!
//! This is where a container stops being bytes and starts being Arrow. It opens a file, checks that
//! the decoder in it is the decoder the file names, compiles it, negotiates with it, asks it for
//! rows, and assembles what comes back into `RecordBatch`es an engine can consume.
//!
//! ```no_run
//! use iris_runtime::Runtime;
//!
//! let bytes = std::fs::read("readings.iris")?;
//! let runtime = Runtime::new()?;
//! let dataset = runtime.open(&bytes)?;
//!
//! println!("{} rows of {}", dataset.rows(), dataset.schema());
//! for batch in dataset.scan()? {
//!     println!("{} rows", batch.num_rows());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A file too large to hold is opened the other way, through a source it stays on.
//!
//! ```no_run
//! use iris_runtime::Runtime;
//! use iris_source::FileSource;
//!
//! let source = FileSource::open("readings.iris".as_ref())?;
//! let runtime = Runtime::new()?;
//! let mut dataset = runtime.open_windowed(Box::new(source))?;
//!
//! println!("{} rows behind a {} byte window", dataset.rows(), dataset.window_bytes());
//! for batch in dataset.scan_rows(0, 1_000)? {
//!     println!("{} rows", batch.num_rows());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Two ways in
//!
//! [`Runtime::open`] takes the whole container as bytes and copies the data section into the guest.
//! It is the fast path for a file that is already in memory, and it stops working at the point the
//! file grows past what a wasm32 guest can address.
//!
//! [`Runtime::open_windowed`] takes a source instead and copies nothing. The metadata is read out
//! of it, the decoder is read out of it and hashed like any other, and the data section is shown to
//! the decoder as a source addressed from zero, so a range the decoder asks for is served out of
//! the file while the file stays where it is. What bounds a request is the window rather than the
//! file, which is why a dataset of any size reads through a window of a fixed one.
//!
//! No decoder changes between them, and that is the claim rather than a convenience. A decoder only
//! ever calls `range` on whatever it was handed, so the two paths differ in what the host offers at
//! the handshake and in nothing the decoder can see.
//!
//! # What it checks
//!
//! Three things, in this order, because the order is the point.
//!
//! The decoder module is hashed and compared against the container before it is compiled. Compiling
//! is the first step that treats those bytes as code, so the check has to come before it rather
//! than alongside it. There is no setting that turns this off, and the reason there is none is
//! structural rather than a matter of nobody having added one: this crate gets the module from
//! `iris-trust`, `iris-trust` hands out the bytes only after hashing them, and there is no other
//! function that returns them.
//!
//! Where the module is allowed to come from is a separate question and has a separate answer. By
//! default it comes from inside the container and nowhere else, because a dataset that names a
//! decoder by URI is asking this host to fetch something and then execute it. A host that means to
//! allow that passes a [`Policy`] carrying a resolver it wrote, and what the resolver returns is
//! hashed like everything else.
//!
//! The ABI the container declares is checked before the module is compiled too, and a refusal names
//! the version, the decoder digest and the schema. Somebody holding a dataset they cannot open needs
//! to know which host would read it, which decoder to go and find, and whether this is even the
//! dataset they were after, and none of that is in scope by the time a refusal comes back out of a
//! guest.
//!
//! The host and the decoder negotiate, and both sides check. A decoder that agrees to terms it
//! cannot meet and a host that runs a decoder it cannot serve are different bugs, and finding out
//! which one happened is worth the second check.
//!
//! Every call into the decoder is metered, and a call that does not come back inside its deadline is
//! stopped. A decoder with an infinite loop in it therefore costs the query it was running and
//! nothing else. There is no setting that turns metering off either, only
//! [`Runtime::with_decoder_deadline`] to move the budget, and the error a stopped decoder produces
//! names it by digest.
//!
//! Every batch goes through `iris-guard` before a single array is built. The guard answers the
//! bounds questions: whether the batch has the arrays and buffers the schema calls for, whether
//! every offset is inside the buffer it indexes, whether a child is long enough for the parent that
//! points into it, and whether a length times a width overflows rather than fits. Then Arrow
//! validates what it is handed as a second opinion from an implementation nobody here wrote. A
//! decoder cannot produce an array that is merely plausible.
//!
//! The schema is checked once when the container is opened rather than once per batch, which is
//! also where a schema nested deeper than anything will walk gets refused. That check comes before
//! the ABI check, because the ABI message describes the schema and formatting a schema you have not
//! checked is how a refusal becomes a crash.
//!
//! # What it does not do yet
//!
//! A windowed scan asks for one range at a time and waits for it. Coalescing neighbouring requests
//! and reading ahead of the decoder are the things that turn that from correct into fast, and they
//! belong on the host side of the boundary because the decoder is not allowed to know how far away
//! its bytes are.
//!
//! Only time is metered. A decoder that allocates until the engine refuses gets an ordinary trap
//! rather than a message about memory, and a decoder that spends its whole deadline on every call
//! and returns is merely slow. Both are worth bounding, and neither of them wedges a host, which is
//! why the deadline was the one that had to come first.
//!
//! Unions, dictionaries, run end encoding and the view types are refused by name rather than
//! skipped. A column that quietly does not arrive is the worst failure this crate could have. The
//! checks the last two of those need are already written in `iris-guard`, so carrying them is a
//! question about the container format rather than about safety.

#![forbid(unsafe_code)]

mod assemble;
mod dataset;
mod error;
mod schema;

pub use dataset::{Dataset, Runtime, Windowed};
pub use error::{Error, Result};
pub use schema::{schema_from_ipc, schema_to_ipc};

/// Not API. The two halves of assembling a batch, so that one can be timed against the other.
///
/// The guard has a cost and the project committed to publishing it before measuring it, which needs
/// the check and the build to be reachable separately. Reaching them through a feature nobody
/// enables by accident is better than either making them public for good or writing a second copy
/// of the assembly path in a probe, since a probe that measures its own copy of the code is
/// measuring the wrong thing the moment the two drift.
///
/// Nothing in here is covered by any stability promise, and a release may remove it.
#[cfg(feature = "probe")]
pub mod probe {
    use arrow_array::RecordBatch;
    use arrow_schema::SchemaRef;
    use iris_vm::RawBatch;

    use crate::Result;

    /// Checks a batch and then builds it, which is what a scan does once per batch.
    ///
    /// # Errors
    ///
    /// Whatever the scan path returns for this batch.
    pub fn record_batch(schema: &SchemaRef, batch: &RawBatch) -> Result<RecordBatch> {
        crate::assemble::record_batch(schema, batch)
    }

    /// Builds a batch that has already been checked, which is the other half of the same work.
    ///
    /// # Errors
    ///
    /// Whatever the build half returns for this batch. Calling this on a batch that has not been
    /// checked is not unsound, because Arrow validates what it is handed, but it is not what the
    /// scan path does and the answer would not mean anything.
    pub fn build(schema: &SchemaRef, batch: &RawBatch) -> Result<RecordBatch> {
        crate::assemble::build(schema, batch)
    }
}

/// Where decoders may come from, and why one was refused.
///
/// Re-exported because [`Error::Trust`] carries an [`Untrusted`] and
/// [`Runtime::with_decoder_policy`] takes a [`Policy`]. A caller that wants to tell a tampered
/// module apart from a container that simply has no decoder in it, or that means to allow a decoder
/// from outside the container, should not have to add a dependency to do it.
pub use iris_trust::{Policy, Resolve, Untrusted};

/// The identity of a decoder, which is the hash of its bytes and nothing else.
///
/// Re-exported because [`Dataset::decoder_digest`] and [`Windowed::decoder_digest`] hand one back. A
/// caller comparing what ran here against what ran somewhere else is comparing these, and it should
/// not have to add a dependency to name the type it is holding.
pub use iris_format::Digest;

/// What a scan cost, in requests to the source and bytes brought back.
///
/// Re-exported for the same reason as [`Digest`]: [`Dataset::last_scan`] and
/// [`Windowed::last_scan`] hand one back, and a host that only wanted to read a container should
/// not have to depend on iris-source to name what it was given.
pub use iris_source::Traffic;

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
