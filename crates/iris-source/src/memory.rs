//! A source over bytes that are already in memory.
//!
//! The simplest implementation there is, and it earns its place three times over. It is what a host
//! uses when the data arrived some other way and is already resident, it is the baseline the
//! windowed and object sources are measured against, and it is the reference the conformance suite
//! was written against: if a check fails here, the check is wrong.

use bytes::Bytes;

use crate::source::{Fetch, RangeSource, SourceError, bounds};

/// A [`RangeSource`] over a buffer that is already resident.
///
/// Every range is served from the buffer, so nothing is ever [`Fetch::Pending`] and nothing can
/// fail except a request that leaves the buffer.
#[derive(Clone, Debug)]
pub struct MemorySource {
    bytes: Bytes,
}

impl MemorySource {
    /// Wraps bytes that are already in memory.
    ///
    /// Taking [`Bytes`] rather than a `Vec` is what lets a host hand the same buffer to several
    /// sources without copying it, which is the case this type is actually for.
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// The buffer this source reads from.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

impl RangeSource for MemorySource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        bounds(at, len, self.len())?;

        // The bounds check passed, so `at` is at most the buffer length and both conversions fit.
        // Anything else would mean a buffer longer than the address space it is stored in.
        let start = usize::try_from(at).unwrap_or(usize::MAX);
        Ok(Fetch::Ready(&self.bytes[start..start + len]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_source_is_never_pending() {
        let mut source = MemorySource::new(&b"abcdef"[..]);
        assert!(source.range(0, 6).expect("in bounds").is_ready());
        assert!(source.range(6, 0).expect("in bounds").is_ready());
    }

    #[test]
    fn an_empty_buffer_is_empty_and_still_serves_a_zero_length_range() {
        let mut source = MemorySource::new(Bytes::new());
        assert!(source.is_empty());
        assert!(matches!(source.range(0, 0), Ok(Fetch::Ready(&[]))));
    }
}
