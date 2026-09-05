//! A byte range of another source, presented as if it were the whole thing.
//!
//! An iris container is a header, a footer, a trailer and the sections in between, and a decoder is
//! only ever shown one of those sections. When the container was resident that was a subslice and
//! there was nothing to write: the host handed over `&bytes[offset..offset + len]` and offset zero
//! meant the start of the data. When the container stays on disk there is no slice to take, so the
//! same shift has to happen on every call instead of once.
//!
//! That is all this is. [`Segment`] adds a fixed offset to every request and reports the length of
//! the range rather than the length of the file, so a decoder that asks for byte zero gets the first
//! byte of its own section and a decoder that asks past the end of its section is out of bounds even
//! though the file continues.
//!
//! Keeping it here rather than in the host is what makes the guarantee hold. A decoder cannot reach
//! outside its section by asking, because the bounds check runs against the section length before
//! the offset is added, and it cannot reach outside it by overflowing, because the addition
//! saturates into a failed bounds check.

use crate::source::{Fetch, RangeSource, SourceError, Traffic, bounds};

/// A range of another source, addressed from zero.
///
/// The inner source is not consumed by this in any lasting way: [`Segment::into_inner`] hands it
/// back, which is what a host reading several sections of one file needs.
#[derive(Clone, Debug)]
pub struct Segment<S> {
    inner: S,
    at: u64,
    len: u64,
}

impl<S: RangeSource> Segment<S> {
    /// Presents `len` bytes of `inner` starting at `at` as a source of its own.
    ///
    /// # Errors
    ///
    /// [`SourceError::OutOfBounds`] if the range is not inside the source, which is checked once
    /// here so that no later call has to check it twice.
    pub fn new(inner: S, at: u64, len: u64) -> Result<Self, SourceError> {
        let end = at.saturating_add(len);
        if end > inner.len() {
            return Err(SourceError::OutOfBounds {
                at,
                end,
                len: inner.len(),
            });
        }
        Ok(Self { inner, at, len })
    }

    /// Where this segment starts in the source underneath it.
    #[must_use]
    pub fn at(&self) -> u64 {
        self.at
    }

    /// The source underneath, for a host that has another section to read.
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: RangeSource> RangeSource for Segment<S> {
    fn len(&self) -> u64 {
        self.len
    }

    fn largest(&self) -> Option<usize> {
        self.inner.largest()
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        // Against the segment, not against the file. A request that runs past the end of the
        // section fails here even when the bytes after it exist and are readable, which is the
        // whole point of the type.
        bounds(at, len, self.len)?;
        self.inner.range(self.at + at, len)
    }

    fn traffic(&self) -> Traffic {
        // The source underneath is the one doing the work, and it is counting the whole file
        // rather than this section. That is the right number: a host asking what a scan cost wants
        // what left the machine, not what left the machine on behalf of one section of a container
        // it read three parts of.
        self.inner.traffic()
    }
}

/// Delegates to a source behind a box, so `Box<dyn RangeSource>` is a source.
///
/// A host that decides which source it is using at run time holds a box, and everything that takes a
/// source generically should still accept it. Without this, [`Segment<Box<dyn RangeSource>>`] does
/// not compile and the boxing has to be undone and redone at every layer.
impl<S: RangeSource + ?Sized> RangeSource for Box<S> {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn largest(&self) -> Option<usize> {
        (**self).largest()
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        (**self).range(at, len)
    }

    fn traffic(&self) -> Traffic {
        (**self).traffic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemorySource;

    fn corpus() -> MemorySource {
        MemorySource::new((0u8..64).collect::<Vec<_>>())
    }

    #[test]
    fn a_segment_is_addressed_from_its_own_start() {
        let mut segment = Segment::new(corpus(), 16, 8).expect("the range is inside the corpus");
        assert_eq!(segment.len(), 8);
        assert!(matches!(
            segment.range(0, 4),
            Ok(Fetch::Ready([16, 17, 18, 19]))
        ));
        assert!(matches!(
            segment.range(4, 4),
            Ok(Fetch::Ready([20, 21, 22, 23]))
        ));
    }

    #[test]
    fn a_request_past_the_end_of_the_segment_fails_even_though_the_bytes_exist() {
        let mut segment = Segment::new(corpus(), 16, 8).expect("the range is inside the corpus");
        assert!(matches!(
            segment.range(4, 8),
            Err(SourceError::OutOfBounds {
                at: 4,
                end: 12,
                len: 8
            })
        ));
        // An offset that would wrap round into the segment saturates into the same failure.
        assert!(matches!(
            segment.range(u64::MAX, 1),
            Err(SourceError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn a_segment_that_leaves_the_source_is_refused_when_it_is_made() {
        assert!(matches!(
            Segment::new(corpus(), 60, 8),
            Err(SourceError::OutOfBounds {
                at: 60,
                end: 68,
                len: 64
            })
        ));
        assert!(matches!(
            Segment::new(corpus(), u64::MAX, 1),
            Err(SourceError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn the_source_underneath_comes_back_for_the_next_section() {
        let segment = Segment::new(corpus(), 16, 8).expect("the range is inside the corpus");
        let mut next = Segment::new(segment.into_inner(), 32, 4).expect("so is this one");
        assert!(matches!(next.range(0, 1), Ok(Fetch::Ready([32]))));
    }

    #[test]
    fn a_boxed_source_is_a_source() {
        let boxed: Box<dyn RangeSource> = Box::new(corpus());
        let mut segment = Segment::new(boxed, 8, 2).expect("the range is inside the corpus");
        assert_eq!(segment.len(), 2);
        assert!(matches!(segment.range(1, 1), Ok(Fetch::Ready([9]))));
    }
}
