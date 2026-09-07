//! What a host has to write to stand in for a decoder, and what can go wrong when it runs.

use core::fmt;
use std::future::Future;
use std::pin::Pin;

use iris_abi::{Hello, RefusalReason, ScanRequest};
use iris_source::RangeSource;
use iris_vm::{Handshake, RawBatch};

/// A native scan that has started and has not finished.
///
/// A boxed future rather than an `async fn` in the trait, because the registry holds
/// `dyn Native` and an `async fn` in a trait cannot be called through a trait object. Writing the
/// boxed form out is the price of dispatch, and it is a price worth paying: the alternative is a
/// synchronous method, and a synchronous method would have every native implementation spin a
/// worker thread on the first range that has not arrived, which is the thing the host above it
/// stopped doing.
pub type Scanning<'a> = Pin<Box<dyn Future<Output = Result<Vec<RawBatch>>> + Send + 'a>>;

/// Host code that decodes a dataset the sandbox would otherwise decode.
///
/// An implementation is a rewrite of a decoder module in native code, and the rewrite is only
/// allowed to run in place of that module. Which module is decided by the digest it was registered
/// against, never by a name, which is what [`Registry`](crate::Registry) is for.
///
/// It is `Send` and `Sync` because one of these is shared by every scan on every thread, exactly
/// the way a compiled module is. An implementation holding per scan state has to keep it in the
/// call rather than in itself, and the signatures here take `&self` so that is not a matter of
/// discipline.
///
/// # What the host still does
///
/// Everything except running the code. The module is still read out of the container and still
/// hashed and checked against what the container claims, because the digest is what selects this
/// implementation in the first place. The batches this produces still go through `iris-guard` and
/// still get validated by Arrow. Substitution replaces the sandbox and nothing else, and the
/// batches a native implementation emits are checked exactly as hard as the ones a guest emits.
pub trait Native: fmt::Debug + Send + Sync {
    /// What this implementation is, in words somebody reading a log can act on.
    ///
    /// It goes into the kernel digest and into the line every substituted scan writes, so a host
    /// that has several kernels can tell which one produced a batch without attaching a debugger to
    /// find out. Something like `fixedwidth-avx2 1.2.0` is the shape of it.
    ///
    /// This one is self asserted and the decoder's name is not, which is not an inconsistency. The
    /// name in a container is written by whoever wrote the container, so trusting it would be
    /// letting a dataset choose which code runs. This is written by whoever installed the native
    /// code, which is the operator, and an operator that lies here is lying to their own logs.
    ///
    /// Worth putting a version in it. Two builds of the same implementation that answer this the
    /// same way are indistinguishable from the outside, because nothing running in a process can
    /// hash the machine code that is executing.
    fn identity(&self) -> &str;

    /// Answers a [`Hello`] the way the module this stands in for would.
    ///
    /// The host negotiates against this answer with the same function it uses on the guest's, so an
    /// implementation that claims a capability it does not have is refused here rather than found
    /// out mid scan.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] if the terms the host offered are not ones this implementation can work
    /// under.
    fn handshake(&self, hello: &Hello) -> Result<Handshake>;

    /// Decodes the rows the request names.
    ///
    /// The source is the bytes of the data section addressed from zero, which is the same view the
    /// guest gets. It is a [`RangeSource`] on both open paths, so an implementation does not have to
    /// know whether the container is resident or is being read a window at a time, and a range that
    /// has not arrived yet is a reason to await rather than a reason to spin.
    ///
    /// The `Hello` is the terms this scan is running under, and it is here rather than being
    /// remembered from the handshake because one of these is shared by every scan on every thread
    /// and has nowhere to remember anything. A guest gets the same numbers, as a session it opens
    /// with, and `max_batch_rows` is the one that shows: a decoder cuts its rows into batches of
    /// that size, so an implementation that ignores it produces the right values in the wrong number
    /// of batches and is not a substitute for the module.
    ///
    /// # Errors
    ///
    /// [`Error::Source`] if a range could not be read, [`Error::Malformed`] if the bytes are not
    /// what this implementation expects, and [`Error::Refused`] if the request asks for something it
    /// agreed to and then could not do.
    fn scan<'a>(
        &'a self,
        hello: &'a Hello,
        request: &'a ScanRequest<'a>,
        source: &'a mut (dyn RangeSource + Send),
    ) -> Scanning<'a>;
}

/// What a native implementation can fail with.
///
/// Deliberately small. A native implementation is host code the operator put there on purpose, so
/// the failures worth naming are the ones a host above it can act on: the bytes were not readable,
/// the bytes were not what this decoder reads, or the terms were wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The implementation will not serve this, in the words the ABI uses for the same answer.
    #[error("the native decoder refused: {reason}: {detail}")]
    Refused {
        /// The reason code, for a caller that wants to branch rather than print.
        reason: RefusalReason,
        /// The text, for whoever reads the log.
        detail: String,
    },

    /// A range the implementation asked for did not arrive.
    #[error(transparent)]
    Source(#[from] iris_source::SourceError),

    /// The bytes are not the ones this implementation reads.
    ///
    /// Reaching this means the digest selected an implementation for a module it does not actually
    /// stand in for, which is either a registration against the wrong bytes or a decoder that reads
    /// a dataset this rewrite has not caught up with.
    #[error("the native decoder could not read this dataset: {0}")]
    Malformed(String),
}

impl Error {
    /// A refusal, for an implementation that will not serve the terms it was offered.
    #[must_use]
    pub fn refused(reason: RefusalReason, detail: impl Into<String>) -> Self {
        Self::Refused {
            reason,
            detail: detail.into(),
        }
    }

    /// Bytes this implementation does not recognise.
    #[must_use]
    pub fn malformed(detail: impl Into<String>) -> Self {
        Self::Malformed(detail.into())
    }
}

/// What this crate returns.
pub type Result<T> = core::result::Result<T, Error>;
