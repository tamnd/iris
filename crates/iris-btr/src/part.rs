//! The framing: a column part, and the chunks inside it.
//!
//! A column part is what the reference writes out for one column of one file, and it is the
//! smallest thing that can be handed to a reader without inventing a container around it. The
//! layout is a count, one offset per chunk, and then the chunks:
//!
//! ```text
//! u32  chunk count
//! u32  offset of chunk 0, from the start of the part
//! ...
//! u32  offset of chunk n-1
//! ...  the chunks, each one a header and then its bytes
//! ```
//!
//! The offsets are not necessarily where the reader would put them. The reference aligns the first
//! chunk, so a part with one chunk has its header end at byte 8 and the chunk start at byte 16.
//! Nothing here assumes otherwise, and nothing here assumes the gap is zero either.
//!
//! Each chunk starts with a twelve byte header:
//!
//! ```text
//! u8   scheme
//! u8   nullmap encoding
//! u8   column type
//! u8   unused
//! u32  where the nullmap starts, counted from the end of this header
//! u32  how many rows the chunk holds
//! ...  the scheme's own bytes, then the nullmap
//! ```
//!
//! The unused byte is padding the reference's struct has and does not write, so it holds whatever
//! was in the buffer. It is read here only to be ignored, which is worth saying out loud because a
//! reader that checked it for zero would pass on one machine and fail on another.

use crate::error::{Error, Result};
use crate::nullmap::Nullmap;

/// What a column holds.
///
/// The reference's enum has eight names and only these three carry data. The other five are
/// placeholders it never writes, so a chunk claiming one of them did not come from the reference
/// and is refused rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnType {
    /// Signed 32 bit integers.
    Integer,
    /// 64 bit floating point.
    Double,
    /// Byte strings, which the reference does not promise are text in any encoding.
    String,
}

impl ColumnType {
    /// Reads the column type byte from a chunk header.
    fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Integer),
            1 => Ok(Self::Double),
            2 => Ok(Self::String),
            _ => Err(Error::UnknownColumnType { code }),
        }
    }
}

impl std::fmt::Display for ColumnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Integer => "integer",
            Self::Double => "double",
            Self::String => "string",
        })
    }
}

/// How long a chunk header is.
const HEADER: usize = 12;

/// A column part, borrowed rather than owned.
///
/// Nothing is copied and nothing is allocated by parsing one. A part that claims a billion chunks
/// has to be large enough to hold a billion offsets before any of them can be read, which is what
/// keeps a hostile count field from turning into a hostile allocation.
#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    bytes: &'a [u8],
    count: u32,
}

impl<'a> Part<'a> {
    /// Reads the part header.
    ///
    /// # Errors
    ///
    /// If the bytes are too short to hold the count, or to hold as many offsets as the count says.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let count = read_u32(bytes, 0, "the chunk count")?;
        // Checked here rather than at every access, and checked against the bytes that are really
        // present rather than against a limit somebody picked.
        let table = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(4))
            .and_then(|table| table.checked_add(4))
            .ok_or(Error::Truncated {
                what: "the offset table",
                from: 4,
                to: usize::MAX,
                len: bytes.len(),
            })?;
        if table > bytes.len() {
            return Err(Error::Truncated {
                what: "the offset table",
                from: 4,
                to: table,
                len: bytes.len(),
            });
        }
        Ok(Self { bytes, count })
    }

    /// How many chunks the part holds.
    #[must_use]
    pub fn chunks(&self) -> u32 {
        self.count
    }

    /// Reads one chunk's header.
    ///
    /// # Errors
    ///
    /// If the index is past the end, if the offsets do not describe chunks laid out end to end, or
    /// if the chunk is too short to hold its own header.
    pub fn chunk(&self, index: u32) -> Result<Chunk<'a>> {
        if index >= self.count {
            return Err(Error::NoSuchChunk {
                index,
                count: self.count,
            });
        }
        let start = self.offset(index)?;
        // The reference records where each chunk begins and never where it ends, so a chunk runs
        // until the next one starts and the last one runs to the end of the part.
        let end = if index + 1 == self.count {
            self.bytes.len()
        } else {
            self.offset(index + 1)?
        };
        if end < start.saturating_add(HEADER) {
            return Err(Error::Overlapping { index, start, end });
        }

        let header = self
            .bytes
            .get(start..start + HEADER)
            .ok_or(Error::Truncated {
                what: "a chunk header",
                from: start,
                to: start + HEADER,
                len: self.bytes.len(),
            })?;
        let body = self
            .bytes
            .get(start + HEADER..end)
            .ok_or(Error::Truncated {
                what: "a chunk",
                from: start + HEADER,
                to: end,
                len: self.bytes.len(),
            })?;

        let scheme = header[0];
        let nullmap_code = header[1];
        let column = ColumnType::from_code(header[2])?;
        // header[3] is padding the reference does not write. See the module documentation.
        let nullmap_at = read_u32(header, 4, "a nullmap offset")?;
        let rows = read_u32(header, 8, "a row count")?;

        let nullmap_at = usize::try_from(nullmap_at).map_err(|_| Error::Overrun {
            what: "the nullmap offset",
            claimed: usize::MAX,
            available: body.len(),
        })?;
        let (data, nullmap) = body.split_at_checked(nullmap_at).ok_or(Error::Overrun {
            what: "the nullmap offset",
            claimed: nullmap_at,
            available: body.len(),
        })?;

        Ok(Chunk {
            column,
            scheme,
            rows,
            data,
            nullmap: Nullmap::new(nullmap_code, rows, nullmap),
        })
    }

    /// The offset of chunk `index`, which the caller has already checked is in range.
    fn offset(&self, index: u32) -> Result<usize> {
        let at = 4 + usize::try_from(index)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let offset = read_u32(self.bytes, at, "a chunk offset")?;
        usize::try_from(offset).map_err(|_| Error::Truncated {
            what: "a chunk offset",
            from: at,
            to: usize::MAX,
            len: self.bytes.len(),
        })
    }
}

/// One chunk of one column.
#[derive(Debug, Clone, Copy)]
pub struct Chunk<'a> {
    column: ColumnType,
    scheme: u8,
    rows: u32,
    data: &'a [u8],
    nullmap: Nullmap<'a>,
}

impl<'a> Chunk<'a> {
    /// What the column holds.
    #[must_use]
    pub fn column(&self) -> ColumnType {
        self.column
    }

    /// The scheme code the chunk was compressed with.
    ///
    /// The same byte means different schemes for different column types, so it only means anything
    /// alongside [`Chunk::column`].
    #[must_use]
    pub fn scheme(&self) -> u8 {
        self.scheme
    }

    /// How many rows the chunk holds, null ones included.
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The scheme's own bytes, without the nullmap after them.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The nullmap, still encoded.
    #[must_use]
    pub fn nullmap(&self) -> Nullmap<'a> {
        self.nullmap
    }
}

/// Reads a little endian `u32` at `at`.
pub(crate) fn read_u32(bytes: &[u8], at: usize, what: &'static str) -> Result<u32> {
    let to = at.checked_add(4).ok_or(Error::Truncated {
        what,
        from: at,
        to: usize::MAX,
        len: bytes.len(),
    })?;
    let field = bytes.get(at..to).ok_or(Error::Truncated {
        what,
        from: at,
        to,
        len: bytes.len(),
    })?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}
