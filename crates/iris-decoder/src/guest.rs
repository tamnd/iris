//! The WebAssembly side of the boundary.
//!
//! Everything in here is compiled only for `wasm32`, and none of it is meant to be called by hand.
//! [`crate::export_decoder`] wires it up.
//!
//! There is no unsafe code here, and that is not an accident of how little the module does. The
//! guest never dereferences an address the host gave it: the host writes into buffers the guest
//! allocated and told it about, so every pointer the guest follows is one it made itself. When
//! sliding windows arrive that stops being free, and the place where it stops being free will be
//! this file rather than anybody's decoder.

use iris_abi::RangeStatus;

use crate::batch::Batch;
use crate::decoder::{Sink, Source};
use crate::error::{Error, Result};

#[link(wasm_import_module = "iris")]
unsafe extern "C" {
    /// Hands the host one batch record. Returns zero if the host took it.
    ///
    /// This is safe to call from the guest's point of view. It passes an address in the guest's own
    /// memory, and what the host does with that address is bounded by the sandbox rather than by
    /// anything this crate promises.
    #[link_name = "emit"]
    safe fn host_emit(ptr: u32, len: u32) -> u32;

    /// Asks the host for bytes the guest does not have, into a buffer the guest allocated.
    ///
    /// Returns an [`iris_abi::RangeStatus`], and only zero means the bytes are there. The call can
    /// take arbitrarily long from the guest's point of view, because the host is allowed to stop
    /// the whole module inside it and carry on later. Nothing in the guest can tell the difference,
    /// which is the point: a decoder writes a straight line and the host decides what waiting means.
    #[link_name = "require_range"]
    safe fn host_require_range(at: u64, len: u32, dst: u32) -> u32;
}

/// Where a slice sits in the guest's memory.
///
/// # Panics
///
/// Panics if the address does not fit in 32 bits, which cannot happen on the only targets this
/// module is compiled for. It is a `try_from` rather than a cast so that a future 64 bit guest
/// fails loudly instead of handing the host half an address.
#[must_use]
pub fn address(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.as_ptr() as usize).expect("a wasm32 address fits in 32 bits")
}

/// A length, narrowed for the host call.
///
/// # Panics
///
/// Panics if the length does not fit in 32 bits, which cannot happen inside a `wasm32` guest whose
/// whole memory is smaller than that.
#[must_use]
pub fn length(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).expect("a wasm32 length fits in 32 bits")
}

/// Widens a length the host sent.
#[must_use]
pub const fn size(len: u32) -> usize {
    len as usize
}

/// Packs an answer into the one value a WebAssembly function can return.
///
/// An empty answer is zero. A real answer is the address in the high half and the length in the
/// low half, and an address is never zero because it always points into a buffer the guest owns.
#[must_use]
pub fn packed(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    (u64::from(address(bytes)) << 32) | u64::from(length(bytes))
}

/// A source that reads what the host already sent and asks for the rest.
///
/// A decoder never chooses between this and [`crate::Resident`], because a decoder never sees either
/// of them: it is handed a `&mut dyn Source` and calls `range`. That indirection is what lets the
/// host change its mind about how much of a file to send without any decoder being rebuilt.
///
/// Ranges inside the bytes the host copied up front are served from those bytes and cost nothing.
/// Everything else goes to `iris.require_range`, which fills the scratch buffer. There is one
/// scratch buffer rather than one per request, so the rule [`Source::range`] already states, that a
/// range may not be held across the next one, is the rule that makes this safe.
#[derive(Debug)]
pub struct HostSource<'a> {
    resident: &'a [u8],
    scratch: &'a mut Vec<u8>,
}

impl<'a> HostSource<'a> {
    /// A source over whatever the host sent, backed by the host for everything else.
    ///
    /// `resident` is allowed to be empty, which is what a host that attached a source rather than
    /// loading one sends, and then every range is a call.
    pub fn new(resident: &'a [u8], scratch: &'a mut Vec<u8>) -> Self {
        Self { resident, scratch }
    }
}

impl Source for HostSource<'_> {
    fn range(&mut self, offset: u64, len: u64) -> Result<&[u8]> {
        // The length has to fit, because the bytes end up in this guest's memory either way. The
        // offset does not, and that difference is the whole of the four gigabyte ceiling: a decoder
        // reading a sixty gigabyte file asks for eight kilobytes at a time from an offset no
        // wasm32 pointer could hold, and the offset stays sixty four bits wide all the way to the
        // import. Narrowing it here would refuse exactly the case a windowed host exists to serve.
        let want = usize::try_from(len)
            .map_err(|_| Error::malformed("a range is longer than this target can address"))?;

        // Looked for in the resident bytes only when the offset is one this target could address,
        // which for a file larger than that it never is: nothing past the ceiling is ever resident,
        // because resident means it is in this guest's memory.
        if let Ok(start) = usize::try_from(offset)
            && let Some(end) = start.checked_add(want)
            && let Some(bytes) = self.resident.get(start..end)
        {
            return Ok(bytes);
        }

        // Sized before the call rather than after, because the host writes into this buffer and a
        // buffer that grows afterwards may have moved. Resizing with zeroes also means a host that
        // returns a status other than served leaves defined bytes behind rather than whatever the
        // last range put there.
        self.scratch.clear();
        self.scratch.resize(want, 0);
        let status = RangeStatus(host_require_range(
            offset,
            length(self.scratch),
            address(self.scratch),
        ));

        match status {
            RangeStatus::SERVED => Ok(self.scratch),
            // The two the decoder can do something about. Asking for a smaller range is a legitimate
            // next move for both, so the error says which one happened rather than saying the source
            // is broken.
            RangeStatus::OUT_OF_BOUNDS => {
                Err(Error::malformed("a range runs off the end of the source"))
            }
            RangeStatus::TOO_LARGE => Err(Error::resource_limit(
                "the host cannot serve a range that long in one request",
            )),
            _ => Err(Error::policy("the host could not serve a range")),
        }
    }
}

/// A sink that hands each batch to the host as it is produced.
#[derive(Debug, Default)]
pub struct HostSink {
    scratch: Vec<u8>,
}

impl HostSink {
    /// A sink that has not emitted anything yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scratch: Vec::new(),
        }
    }
}

impl Sink for HostSink {
    fn emit(&mut self, batch: &Batch) -> Result<()> {
        batch.record(&mut self.scratch)?;
        let status = host_emit(address(&self.scratch), length(&self.scratch));
        if status == 0 {
            Ok(())
        } else {
            // The host does not get to explain itself here, and it does not need to. It already
            // knows why it stopped, and the decoder's only useful response is to stop too.
            Err(Error::policy("the host declined a batch"))
        }
    }
}
