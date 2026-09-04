//! The trait a decoder implements, and the two things it talks to.

use iris_abi::CapabilitySet;

use crate::batch::Batch;
use crate::error::{Error, Result};
use crate::session::{Request, Session};

/// Where a decoder gets bytes of the source.
///
/// The signature is the interesting part. Asking for a range takes `&mut self` and hands back a
/// slice borrowed from it, so holding on to one range while asking for another does not compile.
/// That is not a stylistic preference. Once the host is sliding a window, the bytes behind an
/// earlier range may not be there any more, and "do not keep a pointer across a refill" is a rule
/// that a borrow checker can hold and a comment cannot.
pub trait Source {
    /// Asks the host for `len` bytes starting at `offset` in the source.
    ///
    /// # Errors
    ///
    /// Returns an error if the host declines, which it may do because the range runs off the end of
    /// the source, because it is larger than the window, or because reading it failed.
    fn range(&mut self, offset: u64, len: u64) -> Result<&[u8]>;
}

/// A source whose bytes are all here already.
///
/// This is the whole story for M1, where the host hands the decoder the entire source once and the
/// decoder slices it. There is no host call in this path, which is worth being explicit about: the
/// interesting case is the one where there is, and a decoder written against [`Source`] gets that
/// case for free later without being rebuilt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resident<'a> {
    bytes: &'a [u8],
}

impl<'a> Resident<'a> {
    /// Serves ranges out of a slice.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// How many bytes there are.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether there are no bytes at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Source for Resident<'_> {
    fn range(&mut self, offset: u64, len: u64) -> Result<&[u8]> {
        let start = usize::try_from(offset)
            .map_err(|_| Error::malformed("a range starts past what this target can address"))?;
        let len = usize::try_from(len)
            .map_err(|_| Error::malformed("a range is longer than this target can address"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::malformed("a range's offset and length overflow"))?;
        self.bytes
            .get(start..end)
            .ok_or_else(|| Error::malformed("a range runs off the end of the source"))
    }
}

/// Where a decoder puts the rows it has decoded.
pub trait Sink {
    /// Hands one batch to the host.
    ///
    /// The batch is borrowed rather than moved, so a decoder can keep one and reuse its allocations
    /// with [`Batch::reset`] across a whole scan.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch cannot be described in a record, or if the host declines it.
    fn emit(&mut self, batch: &Batch) -> Result<()>;
}

/// A decoder.
///
/// Everything the ABI needs, other than the decoding itself, is either an associated constant here
/// or is handled by [`crate::Instance`]. A decoder does not encode records, does not negotiate, does
/// not touch guest memory by address, and does not know which WebAssembly functions the host calls.
/// It says what it needs, opens, and decodes.
pub trait Decoder: Sized {
    /// A name for this decoder, for logs and error messages. Nothing interprets it.
    const NAME: &'static str;

    /// What the decoder cannot run without.
    ///
    /// If the host does not offer all of these, the two sides stop before [`Decoder::open`] is
    /// called and the host gets a refusal naming the first capability that was missing.
    const REQUIRES: CapabilitySet = CapabilitySet::new();

    /// What the decoder will use if the host has it and do without if it does not.
    const OPTIONAL: CapabilitySet = CapabilitySet::new();

    /// Called once, after the two sides have agreed on what they can both do.
    ///
    /// A decoder that needs to read a footer in order to know its own shape should do it here, so
    /// that a source it cannot make sense of is a refusal at open rather than a failure partway
    /// through the first scan.
    ///
    /// # Errors
    ///
    /// Returns an error if the decoder cannot work with what the session offers or with what it
    /// finds in the source.
    fn open(session: &Session, source: &mut dyn Source) -> Result<Self>;

    /// Decodes the rows the host asked for, handing them to the sink a batch at a time.
    ///
    /// A decoder may emit as many batches as it likes, including none. Emitting nothing is how a
    /// decoder says the requested rows are past the end of the source, which is not an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the source cannot be read or does not decode.
    fn scan(
        &mut self,
        request: &Request<'_>,
        source: &mut dyn Source,
        sink: &mut dyn Sink,
    ) -> Result<()>;
}
