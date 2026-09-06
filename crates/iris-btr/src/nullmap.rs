//! Which rows are present.
//!
//! The reference stores this four ways. Two of them say the same thing about every row and take no
//! bytes at all, and two of them are a Roaring bitmap of the rows that are the exception. Which of
//! the two bitmap forms is used decides whether the set holds the present rows or the null ones,
//! which is a distinction worth being loud about: reading a flipped map as a regular one gives an
//! answer for every row and gets every one of them backwards.
//!
//! All four are read. The bitmap itself is `CRoaring`'s serialisation rather than anything the
//! reference wrote, so it lives in its own module and this one only decides what the set it hands
//! back means.

use crate::error::{Error, Result};
use crate::roaring;

/// How a nullmap is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Every row is present. No bytes.
    AllPresent,
    /// Every row is null. No bytes.
    AllNull,
    /// A Roaring bitmap of the rows that are present.
    Present,
    /// A Roaring bitmap of the rows that are null.
    Absent,
}

/// A nullmap as it sits in the chunk.
#[derive(Debug, Clone, Copy)]
pub struct Nullmap<'a> {
    code: u8,
    rows: u32,
    bytes: &'a [u8],
}

/// Which rows of a chunk hold a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// Every row.
    All,
    /// None of them.
    None,
    /// One entry per row, true where the row is present.
    Each(Vec<bool>),
}

impl Presence {
    /// Whether `row` holds a value.
    ///
    /// A row past the end of the chunk is not present, which is the same answer as asking about a
    /// null row and is the only answer that makes sense for a row that does not exist.
    #[must_use]
    pub fn present(&self, row: u32) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Each(each) => {
                usize::try_from(row).is_ok_and(|row| *each.get(row).unwrap_or(&false))
            }
        }
    }
}

impl<'a> Nullmap<'a> {
    /// Wraps the bytes after a chunk's scheme data.
    pub(crate) fn new(code: u8, rows: u32, bytes: &'a [u8]) -> Self {
        Self { code, rows, bytes }
    }

    /// The encoding byte from the chunk header.
    #[must_use]
    pub fn code(&self) -> u8 {
        self.code
    }

    /// Works out which rows are present.
    ///
    /// # Errors
    ///
    /// If the encoding is not one the reference defines, or if it is a Roaring bitmap the bytes do
    /// not hold.
    pub fn presence(&self) -> Result<Presence> {
        let kind = self.kind()?;
        let rows = usize::try_from(self.rows).map_err(|_| Error::Overrun {
            what: "the row count",
            claimed: usize::MAX,
            available: self.bytes.len(),
        })?;
        match kind {
            Kind::AllPresent => Ok(Presence::All),
            Kind::AllNull => Ok(Presence::None),
            Kind::Present => Ok(Presence::Each(roaring::read(self.bytes, rows)?)),
            // The set holds the null rows, so every answer is the other way round. Getting this
            // backwards would give a plausible answer for every row and get all of them wrong,
            // which is why the two encodings are separate variants rather than a flag.
            Kind::Absent => {
                let mut each = roaring::read(self.bytes, rows)?;
                for row in &mut each {
                    *row = !*row;
                }
                Ok(Presence::Each(each))
            }
        }
    }

    /// Reads the encoding byte.
    fn kind(&self) -> Result<Kind> {
        match self.code {
            0 => Ok(Kind::AllPresent),
            1 => Ok(Kind::AllNull),
            2 => Ok(Kind::Present),
            3 => Ok(Kind::Absent),
            code => Err(Error::UnknownNullmap { code }),
        }
    }
}
