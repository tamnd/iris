//! The schemes, and the ones that are decoded so far.
//!
//! A scheme code is a byte in the chunk header and it means different things for different column
//! types, so every table here is per type. The names are the reference's own, including the ones it
//! marks as legacy and no longer chooses, because a chunk written by an older copy of it can still
//! carry one and a reader that met one should be able to say what it is rather than say a number.
//!
//! The first two were the two that carry no compression at all, which exercised the whole path from
//! a part on disk to values in hand, for all three column types, without any of the answer depending
//! on a scheme being right. Everything since is a scheme dropped into a frame that already works.
//!
//! # Schemes nest
//!
//! A scheme can hold its output under another scheme. A dictionary keeps its entries and then keeps
//! its codes as an integer column in its own right, and that column carries a scheme byte of its own
//! rather than a chunk header. So decoding is a function per column type taking a code and a slice,
//! calling back into itself, rather than something hanging off a chunk, and `level` is how far down
//! the calls have gone.

use crate::column::{Column, Strings};
use crate::error::{Error, Result};
use crate::fastpfor;
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

/// The code for `DICT`, which is the same for all three column types and means a different layout
/// for strings than it does for the two numeric ones.
const DICT: u8 = 2;

/// The code for `RLE`, which is an integer and a double scheme and is something else entirely for
/// strings.
const RLE: u8 = 3;

/// The code for `BP`, which is an integer scheme and has no counterpart for the other two types.
const BP: u8 = 5;

/// How deep a cascade may go before this gives up on it.
///
/// The reference's own configuration caps a cascade at three, and nothing it writes goes further.
/// This is looser than that on purpose, because refusing a legitimate part is worse than reading a
/// deep one, but it is a cap rather than no cap: schemes nest by calling back into this, and a part
/// that nested a dictionary inside itself forever would otherwise take the stack with it, which is
/// the one failure a caller cannot catch.
const CASCADE: u32 = 8;

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
        let code = self.scheme();
        let rows = usize::try_from(self.rows()).map_err(|_| Error::Overrun {
            what: "the row count",
            claimed: usize::MAX,
            available: self.data().len(),
        })?;

        match self.column() {
            ColumnType::Integer => Ok(Column::Integer(integers(code, self.data(), rows, 0)?)),
            ColumnType::Double => Ok(Column::Double(doubles(code, self.data(), rows, 0)?)),
            ColumnType::String => Ok(Column::Text(text(code, self.data(), rows, 0)?)),
        }
    }
}

/// One of the per column type decoders below.
///
/// A scheme code, the bytes it covers, how many values are in them, and how far down the cascade
/// this is. Named because a scheme that wraps another column has to be able to hand that column to
/// whichever of the three is right for it, and a run length encoding is one shape whichever it gets.
type Decoder<T> = fn(u8, &[u8], usize, u32) -> Result<Vec<T>>;

/// Decodes an integer column held under `code`.
///
/// Split out from [`Chunk::decode`] rather than written inside it because the reference nests
/// schemes. A dictionary stores its codes as an integer column in its own right, with its own
/// scheme byte, and so does a run length encoding, and a nested one has no chunk header around it,
/// only a scheme code recorded by whatever wraps it. So the recursion is on this and `level` counts
/// how far down it has gone.
fn integers(code: u8, data: &[u8], rows: usize, level: u32) -> Result<Vec<i32>> {
    deep_enough(level)?;
    match code {
        UNCOMPRESSED => fixed(data, rows, i32::from_le_bytes),
        ONE_VALUE => Ok(vec![one::<4, i32>(data, i32::from_le_bytes)?; rows]),
        BP => bit_packed(data, rows),
        DICT => dictionary::<4, i32>(data, rows, level, i32::from_le_bytes),
        RLE => run_length(data, rows, level, integers),
        _ => Err(missing(ColumnType::Integer, code)),
    }
}

/// Decodes a double column held under `code`.
fn doubles(code: u8, data: &[u8], rows: usize, level: u32) -> Result<Vec<f64>> {
    deep_enough(level)?;
    match code {
        UNCOMPRESSED => fixed(data, rows, f64::from_le_bytes),
        ONE_VALUE => Ok(vec![one::<8, f64>(data, f64::from_le_bytes)?; rows]),
        DICT => dictionary::<8, f64>(data, rows, level, f64::from_le_bytes),
        RLE => run_length(data, rows, level, doubles),
        _ => Err(missing(ColumnType::Double, code)),
    }
}

/// Decodes a string column held under `code`.
fn text(code: u8, data: &[u8], rows: usize, level: u32) -> Result<Strings> {
    deep_enough(level)?;
    match code {
        UNCOMPRESSED => uncompressed_text(data, rows),
        ONE_VALUE => one_text(data, rows),
        _ => Err(missing(ColumnType::String, code)),
    }
}

/// Whether a cascade has gone as far as this reads.
fn deep_enough(level: u32) -> Result<()> {
    if level > CASCADE {
        return Err(Error::Malformed {
            what: "a cascade of schemes",
            why: "it nests deeper than anything the reference writes",
        });
    }
    Ok(())
}

/// Which of the two errors a scheme code that did not decode deserves.
fn missing(column: ColumnType, code: u8) -> Error {
    if known(column, code) {
        Error::UnsupportedScheme { column, code }
    } else {
        Error::UnknownScheme { column, code }
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

/// Where a dictionary's own bytes start inside the chunk data.
///
/// `DynamicDictionaryStructure` is a scheme byte for the codes, then a four byte offset, then
/// everything else, and the reference declares it packed. Packed is the whole point: without it the
/// compiler would put three bytes of padding after the scheme byte and this would be eight.
const DICTIONARY: usize = 5;

/// Reads a dictionary encoded numeric column.
///
/// The chunk holds the distinct values, then the codes as an integer column in their own right with
/// their own scheme byte. The offset in the header is where the codes start, measured from the end
/// of that header, which means it doubles as the size of the dictionary and is the only thing that
/// says how many entries are in it.
///
/// The reference uses this same structure for integer and for double columns, changing only the
/// width of a dictionary entry, so this is written once and given the width. The codes are integers
/// either way.
///
/// A code that points outside the dictionary is refused. The reference indexes with it directly, so
/// a part that got this wrong would read whatever was next in its address space, and there is no
/// answer to copy here.
fn dictionary<const N: usize, T: Copy>(
    data: &[u8],
    rows: usize,
    level: u32,
    from: fn([u8; N]) -> T,
) -> Result<Vec<T>> {
    let scheme = *data.first().ok_or(Error::Truncated {
        what: "a dictionary",
        from: 0,
        to: 1,
        len: data.len(),
    })?;
    let offset = usize::try_from(read_u32(data, 1, "a dictionary")?).unwrap_or(usize::MAX);

    let entries = data
        .get(DICTIONARY..)
        .and_then(|rest| rest.get(..offset))
        .ok_or(Error::Overrun {
            what: "a dictionary",
            claimed: offset,
            available: data.len().saturating_sub(DICTIONARY),
        })?;
    if entries.len() % N != 0 {
        return Err(Error::Malformed {
            what: "a dictionary",
            why: "its entries do not divide into whole values",
        });
    }
    let dictionary: Vec<T> = entries
        .as_chunks::<N>()
        .0
        .iter()
        .copied()
        .map(from)
        .collect();

    let at = DICTIONARY.saturating_add(offset);
    let rest = data.get(at..).ok_or(Error::Overrun {
        what: "a dictionary",
        claimed: at,
        available: data.len(),
    })?;

    integers(scheme, rest, rows, level + 1)?
        .into_iter()
        .map(|code| {
            usize::try_from(code)
                .ok()
                .and_then(|code| dictionary.get(code))
                .copied()
                .ok_or(Error::Malformed {
                    what: "a dictionary",
                    why: "a code points outside the dictionary",
                })
        })
        .collect()
}

/// Where a run length encoded column's own bytes start inside the chunk data.
///
/// `RLEStructure` is two words and then a scheme byte for the values and a scheme byte for the
/// counts. Unlike the dictionary the reference does not declare this one packed, and it does not
/// need to be: the two words are already aligned and two bytes after them need no padding to sit
/// where they say they do.
const RUNS: usize = 10;

/// Reads a run length encoded column.
///
/// The chunk holds the distinct run values and then the run lengths, each as a column in its own
/// right with its own scheme byte, and the offset in the header says where the lengths start,
/// measured from the end of that header. Both columns hold one entry a run. The lengths are always
/// an integer column whatever the values are, so the values decoder is passed in and the counts are
/// read by this module's integer path either way.
///
/// Two things about the reference are worth knowing here, and neither changes what a reader does.
/// When it compresses, a null row extends the run it is in rather than breaking it, so a run can
/// span rows that hold nothing and the nullmap is what says so. And after it writes the lengths it
/// advances its write pointer by their size twice, with a comment of its own asking why, which
/// leaves the part reporting itself larger than it is. Decoding never notices, because where the
/// lengths are comes from the offset in the header and not from where the values ended.
///
/// The runs have to cover the chunk exactly. The reference writes each one straight into the output
/// with no bound, so a part whose lengths add up to more than the chunk would run it off the end of
/// its own allocation, and one that adds up to less would leave it comparing rows it never wrote.
/// Neither has an answer to copy, so both are refused.
fn run_length<T: Copy>(data: &[u8], rows: usize, level: u32, decode: Decoder<T>) -> Result<Vec<T>> {
    let runs = read_u32(data, 0, "a run length encoded column")?;
    let runs = usize::try_from(runs).unwrap_or(usize::MAX);
    let offset = read_u32(data, 4, "a run length encoded column")?;
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let values_scheme = *data.get(8).ok_or(Error::Truncated {
        what: "a run length encoded column",
        from: 8,
        to: 9,
        len: data.len(),
    })?;
    let counts_scheme = *data.get(9).ok_or(Error::Truncated {
        what: "a run length encoded column",
        from: 9,
        to: 10,
        len: data.len(),
    })?;

    // A run covers at least one row, so there is no reading a part that claims more runs than the
    // chunk has rows. Checked before either nested column is decoded, because the run count is what
    // those two get asked for and it is the field here a part would reach for to ask for work.
    if runs > rows {
        return Err(Error::Malformed {
            what: "a run length encoded column",
            why: "it has more runs than the chunk has rows",
        });
    }

    let body = data.get(RUNS..).ok_or(Error::Overrun {
        what: "a run length encoded column",
        claimed: RUNS,
        available: data.len(),
    })?;
    let at = RUNS.saturating_add(offset);
    let rest = data.get(at..).ok_or(Error::Overrun {
        what: "a run length encoded column",
        claimed: at,
        available: data.len(),
    })?;

    let values = decode(values_scheme, body, runs, level + 1)?;
    let counts = integers(counts_scheme, rest, runs, level + 1)?;

    let mut out = Vec::with_capacity(rows);
    for (value, count) in values.into_iter().zip(counts) {
        let count = usize::try_from(count).map_err(|_| Error::Malformed {
            what: "a run length encoded column",
            why: "a run is a negative number of rows long",
        })?;
        let end = out
            .len()
            .checked_add(count)
            .filter(|end| *end <= rows)
            .ok_or(Error::Malformed {
                what: "a run length encoded column",
                why: "its runs cover more rows than the chunk holds",
            })?;
        out.resize(end, value);
    }
    if out.len() != rows {
        return Err(Error::Malformed {
            what: "a run length encoded column",
            why: "its runs do not cover every row of the chunk",
        });
    }
    Ok(out)
}

/// Reads a bit packed integer column.
///
/// The chunk holds the reference's `XPBPStructure`: a count of the thirty two bit words the codec
/// wrote, then a single padding byte, then the words themselves. The padding is there because the
/// reference aligns the pointer it hands the codec at compression time, so how many bytes it had to
/// skip depends on where that buffer happened to sit in memory. It is recorded in the chunk rather
/// than derived from anything, and a reader has no way to work it out for itself, so this reads it
/// and skips the same number of bytes.
///
/// The values come back as unsigned because that is what the codec deals in. The reference hands it
/// the signed values reinterpreted, so turning them back is a reinterpretation and not a conversion,
/// and a negative value is one that needed all thirty two bits.
fn bit_packed(data: &[u8], rows: usize) -> Result<Vec<i32>> {
    let words = read_u32(data, 0, "a bit packed column")?;
    let words = usize::try_from(words).unwrap_or(usize::MAX);
    let padding = usize::from(*data.get(4).ok_or(Error::Truncated {
        what: "a bit packed column",
        from: 4,
        to: 5,
        len: data.len(),
    })?);

    // Five, not the eight the C++ struct measures, because `data` is declared straight after the
    // padding byte and the trailing bytes that round the struct up to a multiple of four are not
    // part of it.
    let at = 5 + padding;
    let body = data.get(at..).ok_or(Error::Overrun {
        what: "a bit packed column",
        claimed: at,
        available: data.len(),
    })?;

    Ok(fastpfor::binary_packed(body, words, rows)?
        .into_iter()
        .map(u32::cast_signed)
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
