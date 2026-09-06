//! Which rows are present.
//!
//! The reference stores this four ways. Two of them say the same thing about every row and take no
//! bytes at all, and two of them are a Roaring bitmap of the rows that are the exception. Which of
//! the two bitmap forms is used decides whether the set holds the present rows or the null ones,
//! which is a distinction worth being loud about: reading a flipped map as a regular one gives an
//! answer for every row and gets every one of them backwards.
//!
//! Only the two constant forms are read here so far. The Roaring forms wait on the schemes that use
//! them, since every fixture in the corpus that has a scattered nullmap also uses a scheme this
//! crate does not decode yet, and a bitmap reader with nothing to read it for would be untested
//! code that claims to work.

use crate::error::{Error, Result};

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
    /// If the encoding is a Roaring bitmap, which is not read yet, or is not one the reference
    /// defines.
    pub fn presence(&self) -> Result<Presence> {
        match self.kind()? {
            Kind::AllPresent => Ok(Presence::All),
            Kind::AllNull => Ok(Presence::None),
            // The bytes are here and untouched, so this is a missing decoder and not a missing
            // input, which is what the error says.
            Kind::Present | Kind::Absent => {
                let _ = self.bytes;
                let _ = self.rows;
                Err(Error::UnsupportedNullmap { code: self.code })
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
