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

use crate::batch::Batch;
use crate::decoder::Sink;
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
