//! One decoder, and the conversation it is having with a host.
//!
//! This is where every ABI record a decoder deals with is read and written, which is the point.
//! A decoder author never sees a [`Reader`], a [`Writer`] or a tag.

use iris_abi::{ABI_MAJOR, ABI_MINOR, HelloAck, Message, Reader, Refusal, RefusalReason, Writer};

use crate::batch::encode_into;
use crate::decoder::{Decoder, Sink, Source};
use crate::error::{Error, Result};
use crate::session::{Request, Session};

/// A decoder together with the buffers the host writes into and reads out of.
///
/// The host's side of the conversation is three calls: ask for an input buffer, write a record into
/// it, and then ask the instance to act on it. The answer is a record too, and the host reads its
/// tag to find out whether the decoder agreed or refused. There is no separate error channel,
/// because a separate error channel is how a failure ends up as a status code with no explanation
/// attached to it.
#[derive(Debug)]
pub struct Instance<D> {
    decoder: Option<D>,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl<D> Default for Instance<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D> Instance<D> {
    /// A decoder that has not been opened yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            decoder: None,
            input: Vec::new(),
            output: Vec::new(),
        }
    }

    /// Makes room for a record of `len` bytes and hands back where to put it.
    ///
    /// The buffer is reused between calls, so a scan loop that sends a thousand requests does not
    /// allocate a thousand times.
    pub fn input(&mut self, len: usize) -> &mut [u8] {
        self.input.clear();
        self.input.resize(len, 0);
        &mut self.input
    }

    /// Whether the decoder has been opened.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.decoder.is_some()
    }
}

impl<D: Decoder> Instance<D> {
    /// Reads the host's `Hello` out of the input buffer and answers it.
    ///
    /// The answer is a `HelloAck` if the decoder opened and a `Refusal` if it did not. An empty
    /// answer means the decoder could not even encode its own refusal, which a host should treat
    /// the same way it treats a malformed one.
    pub fn start(&mut self, source: &mut dyn Source) -> &[u8] {
        let outcome = self.open(source);
        self.answer(outcome)
    }

    /// Reads the host's `ScanRequest` out of the input buffer and runs it.
    ///
    /// Batches go to the sink as they are produced. The answer is empty when the scan finished and
    /// a `Refusal` when it did not, so a host that gets nothing back knows every batch it was going
    /// to get has already arrived.
    pub fn scan(&mut self, source: &mut dyn Source, sink: &mut dyn Sink) -> &[u8] {
        match self.run(source, sink) {
            Ok(()) => {
                self.output.clear();
                &self.output
            }
            Err(err) => self.refuse(err),
        }
    }

    fn open(&mut self, source: &mut dyn Source) -> Result<HelloAck<'static>> {
        let hello = {
            let mut reader = Reader::new(&self.input);
            let Message::Hello(hello) = reader.message()? else {
                return Err(Error::new(
                    RefusalReason::UNSUPPORTED_RECORD,
                    "a decoder expects a Hello before anything else",
                ));
            };
            hello
        };

        let ack = HelloAck {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            required: D::REQUIRES,
            optional: D::OPTIONAL,
            decoder_id: D::NAME,
        };

        // Both sides run this. The host has to, because it is the one that decides whether to
        // execute anything. The decoder does too, because it is the side that knows what its own
        // refusal means and can say so before it has touched a single byte of the source.
        let agreement = iris_abi::negotiate(&hello, &ack)?;
        let session = Session::new(agreement, hello.source_bytes);
        self.decoder = Some(D::open(&session, source)?);
        Ok(ack)
    }

    fn run(&mut self, source: &mut dyn Source, sink: &mut dyn Sink) -> Result<()> {
        // Destructured so the request can borrow the input buffer while the decoder is borrowed
        // mutably. They are different fields, and saying so is all it takes.
        let Self { decoder, input, .. } = self;

        let mut reader = Reader::new(input);
        let Message::ScanRequest(scan) = reader.message()? else {
            return Err(Error::new(
                RefusalReason::UNSUPPORTED_RECORD,
                "a decoder expects a ScanRequest here",
            ));
        };
        if scan.flags != 0 {
            // Reserved fields are the one lever that lets a later version of the ABI make a change
            // an older decoder must not guess its way through. That only works if this build
            // refuses rather than ignoring them.
            return Err(Error::new(
                RefusalReason::UNSUPPORTED_RECORD,
                "a scan request set a flag this decoder does not know the meaning of",
            ));
        }

        let request = Request::new(scan.row_start, scan.row_count, scan.projection, scan.filter);
        let decoder = decoder.as_mut().ok_or_else(|| {
            Error::new(
                RefusalReason::UNSUPPORTED_RECORD,
                "a scan arrived before the decoder was opened",
            )
        })?;
        decoder.scan(&request, source, sink)
    }

    fn answer(&mut self, outcome: Result<HelloAck<'static>>) -> &[u8] {
        match outcome {
            Ok(ack) => match encode_into(&mut self.output, |w| ack.encode(w)) {
                Ok(()) => &self.output,
                Err(err) => self.refuse(err),
            },
            Err(err) => self.refuse(err),
        }
    }

    fn refuse(&mut self, err: Error) -> &[u8] {
        let refusal = err.as_refusal();
        if encode_into(&mut self.output, |w| refusal.encode(w)).is_err() {
            // The only way this fails is a refusal whose detail string is longer than a megabyte,
            // which cannot happen because every detail string in the tree is a literal. Clearing
            // the buffer rather than panicking keeps a decoder from taking down a scan over a
            // message it was only trying to be helpful with.
            self.output.clear();
        }
        &self.output
    }
}

impl From<Refusal<'static>> for Error {
    fn from(refusal: Refusal<'static>) -> Self {
        // The capability rides along. Dropping it here would turn "this host does not do random
        // access" into "something was missing", which is the difference between a refusal somebody
        // can act on and one they cannot.
        Self::from_refusal(refusal)
    }
}

/// Writes a record into a fresh buffer.
///
/// Hosts and tests both need this in order to build the records they send a decoder, and having one
/// implementation of it means the decoder side and the driving side cannot disagree about framing.
///
/// # Errors
///
/// Returns [`Error::resource_limit`] if the record does not fit in the largest buffer this crate
/// will build.
pub fn record(body: impl Fn(&mut Writer<'_>) -> iris_abi::Result<()>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_into(&mut out, body)?;
    Ok(out)
}
