//! What the host and the decoder settled on, and what the host is asking for.

use iris_abi::{Agreement, Capability, Projection};

/// What the two sides agreed on when the decoder was opened.
///
/// A decoder holds on to this and consults it instead of assuming. A host that does not offer
/// sliding windows is not a host to slide a window against, and finding that out here is much
/// better than finding it out from a refused range in the middle of a scan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Session {
    agreement: Agreement,
    source_bytes: u64,
}

impl Session {
    /// Wraps a negotiated agreement.
    #[must_use]
    pub const fn new(agreement: Agreement, source_bytes: u64) -> Self {
        Self {
            agreement,
            source_bytes,
        }
    }

    /// Whether a capability is in force.
    #[must_use]
    pub const fn has(&self, capability: Capability) -> bool {
        self.agreement.has(capability)
    }

    /// How many bytes of the source the host will keep visible at once, or zero if it will keep all
    /// of them visible and the decoder never has to think about windows.
    #[must_use]
    pub const fn window_bytes(&self) -> u64 {
        self.agreement.window_bytes
    }

    /// The largest number of rows the host will ask for in one scan.
    #[must_use]
    pub const fn max_batch_rows(&self) -> u64 {
        self.agreement.max_batch_rows
    }

    /// How many bytes the whole source has, or zero if the host did not say.
    ///
    /// A decoder that finds its own footer needs this. A decoder that reads forwards from the start
    /// does not, and should not refuse to run just because the host stayed quiet about it.
    #[must_use]
    pub const fn source_bytes(&self) -> u64 {
        self.source_bytes
    }

    /// The minor ABI version both sides settled on, which is the lower of the two.
    #[must_use]
    pub const fn abi_minor(&self) -> u16 {
        self.agreement.abi_minor
    }
}

/// What the host is asking for on one scan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request<'a> {
    row_start: u64,
    row_count: u64,
    projection: Projection<'a>,
    filter: &'a [u8],
}

impl<'a> Request<'a> {
    /// Wraps the fields of a scan request.
    #[must_use]
    pub const fn new(
        row_start: u64,
        row_count: u64,
        projection: Projection<'a>,
        filter: &'a [u8],
    ) -> Self {
        Self {
            row_start,
            row_count,
            projection,
            filter,
        }
    }

    /// The first row wanted, counting from zero.
    #[must_use]
    pub const fn row_start(&self) -> u64 {
        self.row_start
    }

    /// How many rows are wanted. [`u64::MAX`] means everything from [`Request::row_start`] on.
    #[must_use]
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Which columns are wanted, or `None` for all of them.
    ///
    /// The `Option` is the point. An empty projection means every column rather than no columns,
    /// and a decoder that treated it as a list would produce an empty batch and be very hard to
    /// debug, so the type does not let that happen.
    #[must_use]
    pub fn columns(&self) -> Option<impl Iterator<Item = u32> + 'a> {
        if self.projection.is_empty() {
            None
        } else {
            Some(self.projection.iter())
        }
    }

    /// The filter the host pushed down, in whatever form the two sides agreed on. Empty means no
    /// filter, and a decoder that does not do filters may ignore it.
    #[must_use]
    pub const fn filter(&self) -> &'a [u8] {
        self.filter
    }
}
