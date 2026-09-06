//! The `FastPFOR` codecs the reference compresses integer columns with.
//!
//! `BtrBlocks` does not write its own bit packing. It hands the values to Daniel Lemire's `FastPFOR`
//! library and stores whatever that produced, so the bytes in a bit packed chunk are that library's
//! format and not `BtrBlocks`'. This module reads that format.
//!
//! Both of the schemes that use it are a composite of two codecs, where the first handles as many
//! whole blocks as it can and `VariableByte` handles whatever is left over at the end. `BP` puts
//! `FastBinaryPacking<32>` in front, which bit packs a hundred and twenty eight values at a time,
//! and `PFOR` puts `SIMDFastPFor<8>` there, which packs two hundred and fifty six at a time and
//! keeps the values that did not fit at that width to one side. A column whose row count is a
//! multiple of the block size therefore has no tail at all, which is worth knowing when reading a
//! test that seems to cover more than it does.
//!
//! # The two layouts
//!
//! There are two bit packings in here and they are not the same. The ordinary one runs a least
//! significant bit first stream through consecutive words, thirty two values at a time. The
//! vectorised one packs four values at once, so its stream is four independent lanes: word `w`
//! belongs to lane `w % 4`, each lane is the ordinary layout over thirty two values, and lane `j`'s
//! values come back out at positions `4 * i + j`. A reader that took the vectorised words in order
//! would put every value in the wrong slot rather than fail.
//!
//! # The `VariableByte` continuation bit is the other way round
//!
//! Most variable length integer encodings set the high bit of a byte to say another byte follows.
//! This one sets it to say the value ends here. A reader that assumed the usual convention would
//! decode every value in the tail wrongly rather than fail, which is the kind of difference that is
//! only cheap to find if somebody wrote it down.
//!
//! The encoder also pads the tail out to a four byte boundary with zero bytes. A zero byte has the
//! high bit clear, so it reads as a value that never ends, and the reference's own decoder handles
//! that by running out of bytes and dropping the half read value on the floor. This reader instead
//! stops once it has the number of values the chunk said it holds, so the padding is never looked
//! at.

use crate::error::{Error, Result};
use crate::part::read_u32;

/// How many values one packed miniblock holds.
const MINI: usize = 32;

/// How many miniblocks make up a block.
const MINIBLOCKS: usize = 4;

/// How many values a block holds.
const BLOCK: usize = MINI * MINIBLOCKS;

/// How many values the vectorised packer handles at once.
const GROUP: usize = MINI * LANES;

/// How many lanes the vectorised packer interleaves.
const LANES: usize = 4;

/// How many values one `SIMDFastPFor<8>` block holds.
const PATCHED: usize = 256;

/// How many values one `SIMDFastPFor<8>` page holds at most.
const PAGE: usize = 65536;

/// One more than the widest a value can be packed at, so a width can index by itself.
const WIDTHS: usize = 33;

/// Reads the composite of `FastBinaryPacking<32>` and `VariableByte`.
///
/// `data` starts at the first word the codec wrote, `words` is how many thirty two bit words it
/// wrote in total, and `rows` is how many values the chunk says are in there.
pub(crate) fn binary_packed(data: &[u8], words: usize, rows: usize) -> Result<Vec<u32>> {
    let wanted = words.saturating_mul(4);
    let bytes = data.get(..wanted).ok_or(Error::Overrun {
        what: "a bit packed column",
        claimed: wanted,
        available: data.len(),
    })?;

    // The first word is how many values the packing covers, which is the row count rounded down to
    // a whole number of blocks. It is read rather than derived because it is what the reference
    // reads, and a part where the two disagree is a part that does not describe itself.
    let packed = usize::try_from(read_u32(bytes, 0, "a bit packed column")?).unwrap_or(usize::MAX);
    if packed % BLOCK != 0 {
        return Err(Error::Malformed {
            what: "a bit packed column",
            why: "it packs a count that is not a whole number of blocks",
        });
    }
    if packed > rows {
        return Err(Error::Malformed {
            what: "a bit packed column",
            why: "it packs more values than the chunk says it holds",
        });
    }

    // Sized from the row count in the chunk header, which the caller has already checked against
    // the bytes that are there. Nothing here is sized from a length read out of the stream.
    let mut out = Vec::with_capacity(rows);
    let mut at = 4;
    for _ in 0..packed / BLOCK {
        // Four widths in one word, the first miniblock's in the most significant byte, which is
        // what makes reading it as big endian bytes the same thing the reference does with shifts.
        let widths = read_u32(bytes, at, "a bit packed block")?.to_be_bytes();
        at += 4;
        for width in widths {
            let width = usize::from(width);
            if width > 32 {
                return Err(Error::Malformed {
                    what: "a bit packed block",
                    why: "a miniblock is wider than the values it holds",
                });
            }
            out.extend(unpack(bytes, at, 1, width, MINI, "a bit packed miniblock")?);
            at += width * 4;
        }
    }

    variable_byte(bytes, at, rows - packed, &mut out)?;
    Ok(out)
}

/// Unpacks `count` values held at `width` bits each, where `count` is at most thirty two.
///
/// The packed form is a bit stream running least significant bit first through little endian words,
/// so value `i` is the `width` bits starting at bit `i * width`, and a value that reaches the end of
/// a word carries on in the low bits of the next one. A width of zero reads no words at all and
/// means zeroes.
///
/// `stride` is how many words apart this stream's words are. It is one for the ordinary layout and
/// four for the vectorised one, where a lane's words have the other three lanes' words in between
/// them. That is the only difference between the two layouts at this level.
///
/// Asking for fewer than thirty two values reads only the words those values are in, which is what
/// the last part of an exception array needs. The reference unpacks a whole group there and throws
/// away the values it did not ask for, reading past the words that are really there while it does.
fn unpack(
    bytes: &[u8],
    at: usize,
    stride: usize,
    width: usize,
    count: usize,
    what: &'static str,
) -> Result<Vec<u32>> {
    if width == 0 {
        return Ok(vec![0; count]);
    }

    let words = (count * width).div_ceil(32);
    let mut stream = Vec::with_capacity(words);
    for word in 0..words {
        stream.push(read_u32(bytes, at + word * stride * 4, what)?);
    }

    let mask = if width == 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    let mut out = Vec::with_capacity(count);
    for value in 0..count {
        let start = value * width;
        let taken = 32 - start % 32;
        let low = stream[start / 32] >> (start % 32);
        // Only when the value really does cross a word boundary. A value that starts on one takes
        // the whole word, so `taken` is thirty two and this is not reached, which is what keeps the
        // shift below the width of the type. Both indexes are inside `stream` because the words it
        // holds are counted from the same `count` and `width` this loop runs on.
        let high = if taken < width {
            stream[start / 32 + 1] << taken
        } else {
            0
        };
        out.push((low | high) & mask);
    }
    Ok(out)
}

/// Unpacks one vectorised group of a hundred and twenty eight values held at `width` bits each.
///
/// Four lanes side by side, each an ordinary stream of thirty two values, with the lanes' words
/// interleaved word by word and their values interleaved value by value.
fn unpack_group(bytes: &[u8], at: usize, width: usize, out: &mut Vec<u32>) -> Result<()> {
    let base = out.len();
    out.resize(base + GROUP, 0);
    for lane in 0..LANES {
        let values = unpack(
            bytes,
            at + lane * 4,
            LANES,
            width,
            MINI,
            "a vectorised packed group",
        )?;
        for (index, value) in values.into_iter().enumerate() {
            out[base + index * LANES + lane] = value;
        }
    }
    Ok(())
}

/// Reads the composite of `SIMDFastPFor<8>` and `VariableByte`.
///
/// The arguments mean what they do for `binary_packed`, and the framing around both is the same, but
/// the block this one rounds down to is two hundred and fifty six values rather than a hundred and
/// twenty eight, so the two leave different amounts to the tail.
pub(crate) fn patched(data: &[u8], words: usize, rows: usize) -> Result<Vec<u32>> {
    let wanted = words.saturating_mul(4);
    let bytes = data.get(..wanted).ok_or(Error::Overrun {
        what: "a patched column",
        claimed: wanted,
        available: data.len(),
    })?;

    let packed = usize::try_from(read_u32(bytes, 0, "a patched column")?).unwrap_or(usize::MAX);
    if packed % PATCHED != 0 {
        return Err(Error::Malformed {
            what: "a patched column",
            why: "it packs a count that is not a whole number of blocks",
        });
    }
    if packed > rows {
        return Err(Error::Malformed {
            what: "a patched column",
            why: "it packs more values than the chunk says it holds",
        });
    }

    let mut out = Vec::with_capacity(rows);
    let mut at = 4;
    while out.len() < packed {
        let left = packed - out.len();
        at += page(bytes, at, if left > PAGE { PAGE } else { left }, &mut out)?;
    }

    variable_byte(bytes, at, rows - packed, &mut out)?;
    Ok(out)
}

/// Reads one page of a patched column, and says how many bytes of it that took.
///
/// A page is a header word, then the blocks, then the metadata that says how to read them. The
/// header word is how far along the metadata starts, counted in words from the header itself, so the
/// blocks cannot be read until the end of the page has been. That is the whole reason a page exists:
/// the widths and the exceptions are gathered per page rather than per block, so a value that did not
/// fit is packed alongside the other values of its own width from anywhere in the page.
fn page(bytes: &[u8], start: usize, values: usize, out: &mut Vec<u32>) -> Result<usize> {
    let meta = usize::try_from(read_u32(bytes, start, "a patched page")?).unwrap_or(usize::MAX);
    let mut walk = start.saturating_add(meta.saturating_mul(4));

    // A byte for every decision the block loop makes, laid end to end and padded out to a whole
    // number of words. Held as a slice rather than an offset because it is walked a byte at a time
    // and running off the end of it has to be an error rather than a read of the bitmap behind it.
    let size = usize::try_from(read_u32(bytes, walk, "a patched page")?).unwrap_or(usize::MAX);
    walk += 4;
    let mut blocks = bytes
        .get(walk..)
        .and_then(|rest| rest.get(..size))
        .ok_or(Error::Overrun {
            what: "a patched page's block metadata",
            claimed: size,
            available: bytes.len().saturating_sub(walk),
        })?;
    walk += size.div_ceil(4) * 4;

    let widths = read_u32(bytes, walk, "a patched page")?;
    walk += 4;

    // One array a width the page had exceptions at, narrowest first. The bit for a width sits one
    // below it, and there is never one for width one, because an exception a single bit wider than
    // the block it is in is that one bit and gets put back without being stored.
    let mut exceptions: [Vec<u32>; WIDTHS] = std::array::from_fn(|_| Vec::new());
    for (width, array) in exceptions.iter_mut().enumerate().skip(2) {
        if widths & (1u32 << (width - 1)) != 0 {
            walk += exception_array(bytes, walk, width, array)?;
        }
    }

    let mut used = [0usize; WIDTHS];
    let mut at = start + 4;
    for _ in 0..values / PATCHED {
        let width = usize::from(next(&mut blocks)?);
        let count = usize::from(next(&mut blocks)?);
        if width > 32 {
            return Err(Error::Malformed {
                what: "a patched block",
                why: "it packs its values wider than a value can be",
            });
        }

        let base = out.len();
        for _ in 0..PATCHED / GROUP {
            unpack_group(bytes, at, width, out)?;
            at += width * LANES * 4;
        }
        if count == 0 {
            continue;
        }

        // What the exceptions in this block were really as wide as. Everything below `width` is in
        // the block already, so what an exception has to put back is the bits above it, and how many
        // of those there are is which array they were packed into.
        let top = usize::from(next(&mut blocks)?);
        let high = top
            .checked_sub(width)
            .filter(|_| top <= 32)
            .unwrap_or_default();
        if high == 0 {
            return Err(Error::Malformed {
                what: "a patched block",
                why: "it has exceptions that are no wider than the values it packed",
            });
        }

        for _ in 0..count {
            // A position inside the block, which a byte cannot hold too large a value for because a
            // block is two hundred and fifty six values and the loop above just pushed that many.
            let position = base + usize::from(next(&mut blocks)?);
            let patch = if high == 1 {
                1
            } else {
                let value = exceptions[high]
                    .get(used[high])
                    .copied()
                    .ok_or(Error::Malformed {
                        what: "a patched block",
                        why: "it wants more exceptions than the page packed at that width",
                    })?;
                used[high] += 1;
                value
            };
            out[position] |= patch << width;
        }
    }

    // The blocks have to end exactly where the metadata begins. The reference only asserts this, but
    // it is the one check that catches a page header pointing somewhere else and it costs nothing:
    // without it a wrong offset reads the metadata as packed values and answers rather than fails.
    if at != start.saturating_add(meta.saturating_mul(4)) {
        return Err(Error::Malformed {
            what: "a patched page",
            why: "its blocks do not end where it says its metadata starts",
        });
    }
    Ok(walk - start)
}

/// Reads one of the arrays of exceptions that follow a page's metadata, and says how long it was.
///
/// A count, and then that many values packed at `width` bits each: as many vectorised groups of a
/// hundred and twenty eight as fit, then ordinary miniblocks of thirty two, then whatever is left
/// over out of a part word.
fn exception_array(bytes: &[u8], at: usize, width: usize, out: &mut Vec<u32>) -> Result<usize> {
    let count = usize::try_from(read_u32(bytes, at, "an exception array")?).unwrap_or(usize::MAX);
    let start = at + 4;

    // How long the array is, worked out before a single value is read, because `count` came out of
    // the stream and everything below would otherwise size itself from it. Saturating rather than
    // wrapping so that an absurd count comes out too long for the bytes there and is refused.
    let groups = count / GROUP;
    let minis = count % GROUP / MINI;
    let tail = count % MINI;
    let length = groups
        .saturating_mul(LANES)
        .saturating_add(minis)
        .saturating_mul(width)
        .saturating_add((tail * width).div_ceil(32))
        .saturating_mul(4);
    if length > bytes.len().saturating_sub(start) {
        return Err(Error::Overrun {
            what: "an exception array",
            claimed: length,
            available: bytes.len().saturating_sub(start),
        });
    }

    out.reserve(count);
    let mut at = start;
    for _ in 0..groups {
        unpack_group(bytes, at, width, out)?;
        at += width * LANES * 4;
    }
    for _ in 0..minis {
        out.extend(unpack(bytes, at, 1, width, MINI, "an exception array")?);
        at += width * 4;
    }
    if tail > 0 {
        out.extend(unpack(bytes, at, 1, width, tail, "an exception array")?);
    }
    Ok(4 + length)
}

/// Takes the next byte of a page's block metadata.
fn next(blocks: &mut &[u8]) -> Result<u8> {
    let (byte, rest) = blocks.split_first().ok_or(Error::Overrun {
        what: "a patched page's block metadata",
        claimed: 1,
        available: 0,
    })?;
    *blocks = rest;
    Ok(*byte)
}

/// Reads the `wanted` values the composite codec left to `VariableByte`.
///
/// Seven bits a byte, least significant group first, and the high bit set on the last byte of a
/// value rather than on the bytes that continue it.
fn variable_byte(bytes: &[u8], mut at: usize, wanted: usize, out: &mut Vec<u32>) -> Result<()> {
    for _ in 0..wanted {
        let mut value = 0u32;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(at).ok_or(Error::Overrun {
                what: "a variable byte tail",
                claimed: at + 1,
                available: bytes.len(),
            })?;
            at += 1;
            if shift >= 32 {
                return Err(Error::Malformed {
                    what: "a variable byte value",
                    why: "it runs on past the thirty two bits it has to fit in",
                });
            }
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 != 0 {
                break;
            }
            shift += 7;
        }
        out.push(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs `values` at `width` bits each, the way the reference's packer does.
    fn pack_words(values: &[u32], width: usize) -> Vec<u32> {
        if width == 0 {
            return Vec::new();
        }
        let mut words = vec![0u32; (values.len() * width).div_ceil(32)];
        for (index, value) in values.iter().enumerate() {
            let start = index * width;
            words[start / 32] |= value << (start % 32);
            let taken = 32 - start % 32;
            if taken < width {
                words[start / 32 + 1] |= value >> taken;
            }
        }
        words
    }

    /// The same, as the bytes it would be written as.
    fn pack(values: &[u32], width: usize) -> Vec<u8> {
        pack_words(values, width)
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect()
    }

    /// Packs a group of a hundred and twenty eight values the way the vectorised packer does.
    ///
    /// Four lanes, lane `j` taking every fourth value starting at `j`, packed the ordinary way, and
    /// then the four lanes' words laid down one after another in turn.
    fn pack_group(values: &[u32], width: usize) -> Vec<u32> {
        let mut words = vec![0u32; LANES * width];
        for lane in 0..LANES {
            let stream: Vec<u32> = (0..MINI)
                .map(|index| values[index * LANES + lane])
                .collect();
            for (index, word) in pack_words(&stream, width).into_iter().enumerate() {
                words[index * LANES + lane] = word;
            }
        }
        words
    }

    /// The number of values in a block, as the stream records it.
    const PACKED: u32 = 128;

    /// One block of a hundred and twenty eight values, all four miniblocks at the same width.
    fn block(values: &[u32], width: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&PACKED.to_le_bytes());
        let widths = [u8::try_from(width).expect("a width"); MINIBLOCKS];
        bytes.extend_from_slice(&u32::from_be_bytes(widths).to_le_bytes());
        for mini in values.chunks(MINI) {
            bytes.extend_from_slice(&pack(mini, width));
        }
        bytes
    }

    #[test]
    fn a_block_round_trips_at_every_width() {
        // Every width the packer can choose, including the two that are special cased: zero, which
        // writes nothing, and thirty two, which writes the values untouched.
        for width in 0..=32usize {
            let values: Vec<u32> = (0..u32::try_from(BLOCK).expect("a block"))
                .map(|row| {
                    if width == 0 {
                        0
                    } else {
                        let mask = if width == 32 {
                            u32::MAX
                        } else {
                            (1u32 << width) - 1
                        };
                        row.wrapping_mul(2_654_435_761) & mask
                    }
                })
                .collect();
            let bytes = block(&values, width);
            let words = bytes.len() / 4;
            assert_eq!(
                binary_packed(&bytes, words, BLOCK).expect("a block"),
                values,
                "width {width}"
            );
        }
    }

    #[test]
    fn values_that_cross_a_word_boundary_come_back_whole() {
        // Width nine puts a value across a word boundary three times in every miniblock, which is
        // the case a reader that only looked at one word would get wrong.
        let values: Vec<u32> = (0..u32::try_from(BLOCK).expect("a block"))
            .map(|row| row % 512)
            .collect();
        let bytes = block(&values, 9);
        assert_eq!(
            binary_packed(&bytes, bytes.len() / 4, BLOCK).expect("a block"),
            values
        );
    }

    #[test]
    fn a_tail_is_read_with_the_continuation_bit_the_reference_uses() {
        // No packed block at all, so this is the tail on its own: 1, 300, and 100000, each with the
        // high bit set on its last byte rather than on the bytes before it. Then a zero byte of
        // padding, which is never reached because three values were asked for.
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0x81]);
        bytes.extend_from_slice(&[0x2c, 0x82]);
        bytes.extend_from_slice(&[0x20, 0x0d, 0x86]);
        bytes.extend_from_slice(&[0x00, 0x00]);
        assert_eq!(
            binary_packed(&bytes, bytes.len() / 4, 3).expect("a tail"),
            vec![1, 300, 100_000]
        );
    }

    #[test]
    fn a_packed_count_that_is_not_a_whole_number_of_blocks_is_refused() {
        let bytes = 100u32.to_le_bytes();
        assert!(matches!(
            binary_packed(&bytes, 1, 128),
            Err(Error::Malformed {
                what: "a bit packed column",
                ..
            })
        ));
    }

    #[test]
    fn a_packed_count_larger_than_the_chunk_allocates_nothing() {
        // The count is a whole number of blocks and enormous. Sizing the output from it would ask
        // for four gigabytes on a chunk of four bytes.
        let bytes = (u32::MAX - 127).to_le_bytes();
        assert!(matches!(
            binary_packed(&bytes, 1, 128),
            Err(Error::Malformed {
                what: "a bit packed column",
                ..
            })
        ));
    }

    #[test]
    fn a_miniblock_wider_than_a_value_is_refused() {
        let mut bytes = PACKED.to_le_bytes().to_vec();
        bytes.extend_from_slice(&u32::from_be_bytes([33, 0, 0, 0]).to_le_bytes());
        assert!(matches!(
            binary_packed(&bytes, bytes.len() / 4, BLOCK),
            Err(Error::Malformed {
                what: "a bit packed block",
                ..
            })
        ));
    }

    #[test]
    fn a_block_that_says_more_than_it_holds_stops() {
        // One block's worth of values claimed, sixteen bits wide, and none of the words there.
        let mut bytes = PACKED.to_le_bytes().to_vec();
        bytes.extend_from_slice(&u32::from_be_bytes([16; MINIBLOCKS]).to_le_bytes());
        assert!(matches!(
            binary_packed(&bytes, bytes.len() / 4, BLOCK),
            Err(Error::Truncated { .. })
        ));
    }

    /// The mask a block packed at `width` keeps.
    fn mask(width: usize) -> u32 {
        if width == 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        }
    }

    /// Builds one patched page out of `values`, packing block `n` at `widths[n]`.
    ///
    /// Whatever does not fit at the width its block was given becomes an exception, which is how the
    /// reference gets a page with exceptions in it too: it chooses the width that makes the page
    /// smallest and takes the outliers that leaves. A test picks the width instead, so that it can
    /// ask for a page with no exceptions at all, or one where every block has them.
    fn page_of(values: &[u32], widths: &[usize]) -> Vec<u8> {
        let mut blocks = Vec::new();
        let mut packed = Vec::new();
        let mut arrays: [Vec<u32>; WIDTHS] = std::array::from_fn(|_| Vec::new());

        for (block, &width) in values.chunks(PATCHED).zip(widths) {
            let over: Vec<(usize, u32)> = block
                .iter()
                .enumerate()
                .filter(|(_, value)| **value & !mask(width) != 0)
                .map(|(position, value)| (position, value >> width))
                .collect();
            let top = over
                .iter()
                .map(|(_, high)| 32 - high.leading_zeros() as usize)
                .max()
                .unwrap_or_default()
                + width;

            for group in block.chunks(GROUP) {
                let low: Vec<u32> = group.iter().map(|value| value & mask(width)).collect();
                packed.extend(pack_group(&low, width));
            }

            blocks.push(u8::try_from(width).expect("a width"));
            blocks.push(u8::try_from(over.len()).expect("an exception count"));
            if !over.is_empty() {
                blocks.push(u8::try_from(top).expect("a width"));
                for (position, high) in over {
                    blocks.push(u8::try_from(position).expect("a position"));
                    if top - width > 1 {
                        arrays[top - width].push(high);
                    }
                }
            }
        }

        // The page header says where the metadata starts, counted in words from the header itself,
        // so it is one for the header plus however many words the blocks came to.
        let mut page = Vec::new();
        page.extend_from_slice(
            &u32::try_from(1 + packed.len())
                .expect("a page")
                .to_le_bytes(),
        );
        page.extend(packed.iter().flat_map(|word| word.to_le_bytes()));

        page.extend_from_slice(&u32::try_from(blocks.len()).expect("a size").to_le_bytes());
        page.extend_from_slice(&blocks);
        page.resize(
            page.len() + (blocks.len().div_ceil(4) * 4 - blocks.len()),
            0,
        );

        let mut bitmap = 0u32;
        for (width, array) in arrays.iter().enumerate() {
            if !array.is_empty() {
                bitmap |= 1u32 << (width - 1);
            }
        }
        page.extend_from_slice(&bitmap.to_le_bytes());
        for (width, array) in arrays.iter().enumerate() {
            if array.is_empty() {
                continue;
            }
            page.extend_from_slice(&u32::try_from(array.len()).expect("a count").to_le_bytes());
            let mut done = 0;
            while done + GROUP <= array.len() {
                page.extend(
                    pack_group(&array[done..done + GROUP], width)
                        .iter()
                        .flat_map(|word| word.to_le_bytes()),
                );
                done += GROUP;
            }
            while done < array.len() {
                let end = (done + MINI).min(array.len());
                page.extend(pack(&array[done..end], width));
                done = end;
            }
        }
        page
    }

    /// A whole patched column: the packed count, then the pages.
    fn patched_column(values: &[u32], widths: &[usize]) -> Vec<u8> {
        let packed = values.len() / PATCHED * PATCHED;
        let mut bytes = u32::try_from(packed)
            .expect("a count")
            .to_le_bytes()
            .to_vec();
        for (page, widths) in values[..packed]
            .chunks(PAGE)
            .zip(widths.chunks(PAGE / PATCHED))
        {
            bytes.extend(page_of(page, widths));
        }
        bytes
    }

    #[test]
    fn a_patched_block_round_trips_at_every_width() {
        // The vectorised layout is the thing under test here. A reader that took its words in order
        // rather than four lanes at a time would come back with every value in the wrong slot, and
        // at width thirty two it would come back with the right multiset in the wrong order, which
        // is why the values are all different from each other.
        for width in 0..=32usize {
            let values: Vec<u32> = (0..u32::try_from(PATCHED).expect("a block"))
                .map(|row| row.wrapping_mul(2_654_435_761) & mask(width))
                .collect();
            let bytes = patched_column(&values, &[width]);
            assert_eq!(
                patched(&bytes, bytes.len() / 4, PATCHED).expect("a block"),
                values,
                "width {width}"
            );
        }
    }

    #[test]
    fn an_exception_a_single_bit_wider_is_put_back_without_being_stored() {
        // The one case the reference never writes an array for: the exceptions are one bit above the
        // width the block packed at, so putting them back is setting that bit and there is nothing
        // to store. A reader that looked for an array here would find whatever came next instead.
        let values: Vec<u32> = (0..u32::try_from(PATCHED).expect("a block"))
            .map(|row| {
                if row % 17 == 0 {
                    16 + row % 16
                } else {
                    row % 16
                }
            })
            .collect();
        let bytes = patched_column(&values, &[4]);
        assert_eq!(
            patched(&bytes, bytes.len() / 4, PATCHED).expect("a block"),
            values
        );
    }

    #[test]
    fn exceptions_come_out_of_the_array_for_the_width_they_were_packed_at() {
        // Three blocks, all packed four bits wide. The first and third have exceptions seven bits
        // wide and the second has them six, so the page holds two arrays and the third block has to
        // carry on through the first array from where the first block left off rather than start it
        // again. Reading the arrays per block instead of per page is the mistake this catches.
        let mut values: Vec<u32> = (0..u32::try_from(PATCHED * 3).expect("three blocks"))
            .map(|row| row % 16)
            .collect();
        for (row, value) in [
            (5, 0x41),
            (200, 0x7e),
            (PATCHED + 3, 0x2c),
            (PATCHED * 2 + 9, 0x55),
        ] {
            values[row] = value;
        }
        let bytes = patched_column(&values, &[4, 4, 4]);
        assert_eq!(
            patched(&bytes, bytes.len() / 4, PATCHED * 3).expect("three blocks"),
            values
        );
    }

    #[test]
    fn an_exception_array_is_read_past_its_last_whole_group() {
        // A hundred and thirty exceptions at one width, which is a vectorised group of a hundred and
        // twenty eight and then two values in a part word. The reference unpacks a whole group of
        // thirty two for those two and throws away the rest, reading words that are not there while
        // it does, so the length it comes out with is the thing worth checking against.
        let values: Vec<u32> = (0..u32::try_from(PATCHED).expect("a block"))
            .map(|row| if row < 130 { 0x300 + row } else { row % 8 })
            .collect();
        let bytes = patched_column(&values, &[3]);
        assert_eq!(
            patched(&bytes, bytes.len() / 4, PATCHED).expect("a block"),
            values
        );
    }

    #[test]
    fn a_column_of_more_than_one_page_reads_every_page() {
        // A page holds sixty five thousand five hundred and thirty six values, so this is one full
        // page and one block. Nothing else here would notice a reader that decoded the first page
        // and stopped, or one that started the second page in the wrong place.
        let values: Vec<u32> = (0..u32::try_from(PAGE + PATCHED).expect("a column"))
            .map(|row| row % 3)
            .collect();
        let widths = vec![2; values.len() / PATCHED];
        let bytes = patched_column(&values, &widths);
        assert_eq!(
            patched(&bytes, bytes.len() / 4, values.len()).expect("two pages"),
            values
        );
    }

    #[test]
    fn a_patched_column_reads_the_values_the_block_size_left_over() {
        // The tail is the same codec as it is for bit packing, but the block it is what is left over
        // from is twice as long, so a column of two hundred values is all tail here and would be one
        // packed block and seventy two values of tail there.
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0x81, 0x2c, 0x82, 0x00]);
        assert_eq!(
            patched(&bytes, bytes.len() / 4, 2).expect("a tail"),
            vec![1, 300]
        );
    }

    #[test]
    fn a_patched_count_that_is_not_a_whole_number_of_blocks_is_refused() {
        // A hundred and twenty eight is a whole number of blocks for bit packing and not for this.
        let bytes = 128u32.to_le_bytes();
        assert!(matches!(
            patched(&bytes, 1, 256),
            Err(Error::Malformed {
                what: "a patched column",
                ..
            })
        ));
    }

    #[test]
    fn a_patched_count_larger_than_the_chunk_allocates_nothing() {
        let bytes = (u32::MAX - 255).to_le_bytes();
        assert!(matches!(
            patched(&bytes, 1, 256),
            Err(Error::Malformed {
                what: "a patched column",
                ..
            })
        ));
    }

    #[test]
    fn blocks_that_do_not_end_where_the_metadata_starts_are_refused() {
        // The page header is the only thing saying where the blocks stop, and nothing about the
        // blocks themselves would disagree with it, so a page that pointed into the middle of them
        // would decode the metadata as values and answer rather than fail.
        let values: Vec<u32> = (0..u32::try_from(PATCHED).expect("a block")).collect();
        let mut bytes = patched_column(&values, &[9]);

        // A spare word slipped in between the blocks and the metadata, and a header saying to skip
        // over it. Everything still parses and every value still comes back, and the only thing that
        // is wrong is that the blocks stopped a word earlier than the page said they would.
        let header = u32::from_le_bytes(bytes[4..8].try_into().expect("a header"));
        let at = 4 + usize::try_from(header).expect("a header") * 4;
        bytes.splice(at..at, [0; 4]);
        bytes[4..8].copy_from_slice(&(header + 1).to_le_bytes());
        assert!(matches!(
            patched(&bytes, bytes.len() / 4, PATCHED),
            Err(Error::Malformed {
                what: "a patched page",
                ..
            })
        ));
    }

    #[test]
    fn a_block_wider_than_a_value_is_refused() {
        // Two metadata bytes saying a width of thirty three and no exceptions, and a page header
        // pointing straight at them.
        let mut bytes = 256u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[33, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            patched(&bytes, bytes.len() / 4, PATCHED),
            Err(Error::Malformed {
                what: "a patched block",
                ..
            })
        ));
    }

    #[test]
    fn exceptions_no_wider_than_the_block_they_are_in_are_refused() {
        // A block packed at nothing at all with one exception said to be no bits wide, which would
        // leave nothing to put back and would index the array a width below the narrowest there is.
        let mut bytes = 256u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 1, 0, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            patched(&bytes, bytes.len() / 4, PATCHED),
            Err(Error::Malformed {
                what: "a patched block",
                ..
            })
        ));
    }

    #[test]
    fn a_page_that_runs_out_of_block_metadata_is_refused() {
        // Two blocks to read and one block's worth of bytes to read them from. The bytes after the
        // container are the bitmap, and a reader that walked on into it would take that as a width.
        let mut bytes = 512u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            patched(&bytes, bytes.len() / 4, 512),
            Err(Error::Overrun {
                what: "a patched page's block metadata",
                ..
            })
        ));
    }

    #[test]
    fn an_exception_array_longer_than_the_page_allocates_nothing() {
        // An array of four billion values at two bits each, which is a gigabyte of words asked for
        // by four bytes. Its length has to be worked out before a single value is read.
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            page(&bytes, 4, 0, &mut Vec::new()),
            Err(Error::Overrun {
                what: "an exception array",
                ..
            })
        ));
    }

    #[test]
    fn a_tail_that_never_ends_is_refused() {
        // Five continuation bytes with the high bit clear, which is more than thirty two bits of
        // payload and therefore not a value this can hold.
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f]);
        assert!(matches!(
            binary_packed(&bytes, bytes.len() / 4, 1),
            Err(Error::Malformed {
                what: "a variable byte value",
                ..
            })
        ));
    }
}
