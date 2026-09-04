//! One batch, copied out of the guest.

use iris_abi::Node;

/// A batch of decoded rows, with the buffers copied out of the guest's memory.
///
/// The copy is the point of the type. A decoder is allowed to reuse its buffers between batches,
/// which means the bytes an offset points at are only valid while the guest is still inside the
/// `emit` call that produced them. Anything that wants to look at them later has to take them, so
/// this crate takes them once, at the only moment they are certainly there.
///
/// That copy is not free and it is not permanent. It exists because M1 is about the contract rather
/// than the throughput, and the thing that removes it is a decoder that promises not to reuse a
/// buffer, which is a capability bit and a later milestone rather than a change here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawBatch {
    /// How many rows the batch holds.
    pub rows: u64,
    /// One entry per array, in schema pre-order.
    pub nodes: Vec<Node>,
    /// One entry per buffer, in schema pre-order.
    pub buffers: Vec<Vec<u8>>,
}

impl RawBatch {
    /// How many bytes the buffers came to.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.buffers.iter().map(Vec::len).sum()
    }
}
