//! What comes out of a chunk.

/// A decoded chunk.
///
/// One variant per column type the reference writes. The values for null rows are whatever the
/// scheme put there, which for some schemes is a real value and for others is undefined. That is
/// the reference's behaviour and repeating it is the point, so nothing here is zeroed on the way
/// out. Ask [`crate::Chunk::nullmap`] which rows meant anything.
///
/// Not `non_exhaustive`. The reference's column type enum has eight names and these are the three
/// that carry data, so this is the whole set rather than the set so far, and a caller matching on
/// it should not have to write an arm for a case that cannot happen.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// A column of signed 32 bit integers, one per row.
    Integer(Vec<i32>),
    /// A column of doubles, one per row.
    Double(Vec<f64>),
    /// A column of byte strings, one per row.
    Text(Strings),
}

/// A column of byte strings.
///
/// Held the way a columnar reader wants them, which is all the bytes in one buffer and an offset
/// per row plus one on the end, so that every length is a subtraction and no row owns anything.
/// Offsets count from the start of [`Strings::bytes`].
///
/// Not `String`, and not checked for UTF-8. The reference stores byte strings and says nothing
/// about their encoding, so a reader that insisted on UTF-8 would refuse files the reference is
/// happy to write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Strings {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Strings {
    /// Builds a column from offsets and bytes.
    ///
    /// # Panics
    ///
    /// If `offsets` is empty. A string column always has one more offset than it has rows, so an
    /// empty offset list is not an empty column, it is a caller that lost the terminator.
    #[must_use]
    pub fn new(offsets: Vec<u32>, bytes: Vec<u8>) -> Self {
        assert!(
            !offsets.is_empty(),
            "a string column needs a terminating offset"
        );
        Self { offsets, bytes }
    }

    /// How many rows there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The offsets, one per row plus one on the end.
    #[must_use]
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Every row's bytes, run together.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// One row, or `None` past the end.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<&[u8]> {
        let from = usize::try_from(*self.offsets.get(row)?).ok()?;
        let to = usize::try_from(*self.offsets.get(row + 1)?).ok()?;
        self.bytes.get(from..to)
    }
}
