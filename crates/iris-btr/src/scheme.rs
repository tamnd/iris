//! The schemes, and the two of them that are decoded so far.
//!
//! A scheme code is a byte in the chunk header and it means different things for different column
//! types, so every table here is per type. The names are the reference's own, including the ones it
//! marks as legacy and no longer chooses, because a chunk written by an older copy of it can still
//! carry one and a reader that met one should be able to say what it is rather than say a number.
//!
//! Two are implemented, and they are the two that carry no compression at all. That is deliberate
//! for a first pass: they exercise the whole path from a part on disk to values in hand, for all
//! three column types, without any of the answer depending on a scheme being right. Everything
//! after this is a scheme dropped into a frame that already works.

use crate::column::{Column, Strings};
use crate::error::{Error, Result};
use crate::part::{Chunk, ColumnType, read_u32};

/// The reference's name for a scheme code, or `"unknown"`.
///
/// Used to make an error readable. It is not a promise that the scheme is implemented, only that
/// the reference has a name for it.
#[must_use]
pub fn scheme_name(column: ColumnType, code: u8) -> &'static str {
    match column {
        ColumnType::Integer => match code {
            0 => "UNCOMPRESSED",
            1 => "ONE_VALUE",
            2 => "DICT",
            3 => "RLE",
            4 => "PFOR",
            5 => "BP",
            25 => "FREQUENCY",
            26 => "FOR",
            27 => "PFOR_DELTA",
            28 => "TRUNCATION_8",
            29 => "TRUNCATION_16",
            30 => "DICTIONARY_8",
            31 => "DICTIONARY_16",
            _ => "unknown",
        },
        ColumnType::Double => match code {
            0 => "UNCOMPRESSED",
            1 => "ONE_VALUE",
            2 => "DICT",
            3 => "RLE",
            4 => "FREQUENCY",
            5 => "PSEUDODECIMAL",
            28 => "DOUBLE_BP",
            29 => "DICTIONARY_8",
            31 => "DICTIONARY_16",
            _ => "unknown",
        },
        ColumnType::String => match code {
            0 => "UNCOMPRESSED",
            1 => "ONE_VALUE",
            2 => "DICT",
            3 => "FSST",
            30 => "DICTIONARY_8",
            31 => "DICTIONARY_16",
            _ => "unknown",
        },
    }
}

/// Whether the reference defines this code for this column type.
fn known(column: ColumnType, code: u8) -> bool {
    scheme_name(column, code) != "unknown"
}

/// The code for `UNCOMPRESSED`, which is the same for all three column types.
const UNCOMPRESSED: u8 = 0;

/// The code for `ONE_VALUE`, likewise.
const ONE_VALUE: u8 = 1;

impl Chunk<'_> {
    /// Decodes the chunk.
    ///
    /// The values for null rows are left as the scheme wrote them, which for some schemes is a real
    /// value and for others is undefined. Read [`Chunk::nullmap`] to find out which rows meant
    /// anything.
    ///
    /// # Errors
    ///
    /// If the scheme is not implemented yet, if it is not one the reference defines, or if the
    /// chunk is too short for what the scheme says is in it.
    pub fn decode(&self) -> Result<Column> {
        let column = self.column();
        let code = self.scheme();
        let rows = usize::try_from(self.rows()).map_err(|_| Error::Overrun {
            what: "the row count",
            claimed: usize::MAX,
            available: self.data().len(),
        })?;

        match (column, code) {
            (ColumnType::Integer, UNCOMPRESSED) => Ok(Column::Integer(fixed(
                self.data(),
                rows,
                i32::from_le_bytes,
            )?)),
            (ColumnType::Double, UNCOMPRESSED) => Ok(Column::Double(fixed(
                self.data(),
                rows,
                f64::from_le_bytes,
            )?)),
            (ColumnType::String, UNCOMPRESSED) => {
                Ok(Column::Text(uncompressed_text(self.data(), rows)?))
            }

            (ColumnType::Integer, ONE_VALUE) => Ok(Column::Integer(vec![
                one::<4, i32>(
                    self.data(),
                    i32::from_le_bytes
                )?;
                rows
            ])),
            (ColumnType::Double, ONE_VALUE) => Ok(Column::Double(vec![
                one::<8, f64>(
                    self.data(),
                    f64::from_le_bytes
                )?;
                rows
            ])),
            (ColumnType::String, ONE_VALUE) => Ok(Column::Text(one_text(self.data(), rows)?)),

            _ if known(column, code) => Err(Error::UnsupportedScheme { column, code }),
            _ => Err(Error::UnknownScheme { column, code }),
        }
    }
}

/// Reads `rows` fixed width values laid out back to back.
///
/// The reference writes these with a `memcpy` from the caller's array, so they are exactly the host
/// bytes and nothing else. Read here as little endian rather than transmuted, since a part is a
/// file and a file that travelled between machines should not decode differently on each.
fn fixed<const N: usize, T>(data: &[u8], rows: usize, from: fn([u8; N]) -> T) -> Result<Vec<T>> {
    let wanted = rows.checked_mul(N).ok_or(Error::Overrun {
        what: "an uncompressed column",
        claimed: usize::MAX,
        available: data.len(),
    })?;
    let values = data.get(..wanted).ok_or(Error::Overrun {
        what: "an uncompressed column",
        claimed: wanted,
        available: data.len(),
    })?;
    // The slice was cut to a whole number of values above, so the remainder `as_chunks` hands back
    // is empty and there is nothing to decide about it.
    Ok(values
        .as_chunks::<N>()
        .0
        .iter()
        .copied()
        .map(from)
        .collect())
}

/// Reads the single value a `ONE_VALUE` chunk holds.
fn one<const N: usize, T>(data: &[u8], from: fn([u8; N]) -> T) -> Result<T> {
    let value = data.get(..N).ok_or(Error::Overrun {
        what: "a one value column",
        claimed: N,
        available: data.len(),
    })?;
    Ok(from(value.try_into().unwrap_or([0; N])))
}

/// Reads an uncompressed string column.
///
/// The chunk holds a length and then the reference's own layout for a string column: one offset per
/// row plus one on the end, then the bytes, with each offset counted from the start of the offset
/// array rather than from the start of the bytes. Converting that to offsets into the bytes alone
/// is this function's whole job, and it is where a reader that assumed the two were the same would
/// come out with every string shifted by the length of the offset array.
fn uncompressed_text(data: &[u8], rows: usize) -> Result<Strings> {
    let total = read_u32(data, 0, "an uncompressed string column")?;
    let total = usize::try_from(total).unwrap_or(usize::MAX);
    let body = data
        .get(4..)
        .and_then(|body| body.get(..total))
        .ok_or(Error::Overrun {
            what: "an uncompressed string column",
            claimed: total,
            available: data.len().saturating_sub(4),
        })?;

    let header = rows
        .checked_add(1)
        .and_then(|slots| slots.checked_mul(4))
        .ok_or(Error::Overrun {
            what: "a string offset array",
            claimed: usize::MAX,
            available: body.len(),
        })?;
    if header > body.len() {
        return Err(Error::Overrun {
            what: "a string offset array",
            claimed: header,
            available: body.len(),
        });
    }

    let mut offsets = Vec::with_capacity(rows + 1);
    for row in 0..=rows {
        let slot = read_u32(body, row * 4, "a string offset")?;
        let slot = usize::try_from(slot).unwrap_or(usize::MAX);
        // Rebased onto the bytes. A slot pointing into the offset array itself, or past the end of
        // what the chunk holds, is a part that does not describe itself.
        let rebased = slot
            .checked_sub(header)
            .filter(|at| *at <= body.len() - header)
            .ok_or(Error::Overrun {
                what: "a string offset",
                claimed: slot,
                available: body.len(),
            })?;
        offsets.push(u32::try_from(rebased).unwrap_or(u32::MAX));
    }

    Ok(Strings::new(offsets, body[header..].to_vec()))
}

/// Reads a string column where every row holds the same string.
///
/// A length and then that many bytes. There is no room in that for a null row to hold anything
/// different, so a null row gets the same string as every other one, and the nullmap is what says
/// it did not mean it.
fn one_text(data: &[u8], rows: usize) -> Result<Strings> {
    let length = read_u32(data, 0, "a one value string column")?;
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    let value = data
        .get(4..)
        .and_then(|rest| rest.get(..length))
        .ok_or(Error::Overrun {
            what: "a one value string column",
            claimed: length,
            available: data.len().saturating_sub(4),
        })?;

    let mut offsets = Vec::with_capacity(rows + 1);
    let mut bytes = Vec::with_capacity(length.saturating_mul(rows));
    for row in 0..=rows {
        offsets.push(u32::try_from(row.saturating_mul(length)).unwrap_or(u32::MAX));
        if row != rows {
            bytes.extend_from_slice(value);
        }
    }
    Ok(Strings::new(offsets, bytes))
}
