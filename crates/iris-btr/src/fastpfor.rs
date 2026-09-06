//! The `FastPFOR` codecs the reference compresses integer columns with.
//!
//! `BtrBlocks` does not write its own bit packing. It hands the values to Daniel Lemire's `FastPFOR`
//! library and stores whatever that produced, so the bytes in a bit packed chunk are that library's
//! format and not `BtrBlocks`'. This module reads that format.
//!
//! What the `BP` scheme uses is a composite of two codecs. `FastBinaryPacking<32>` takes the values
//! in blocks of a hundred and twenty eight and bit packs them, and `VariableByte` takes whatever is
//! left over at the end, since the first codec only handles whole blocks. A column whose row count
//! is a multiple of a hundred and twenty eight therefore has no tail at all, which is worth knowing
//! when reading a test that seems to cover more than it does.
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
            unpack(bytes, at, width, &mut out)?;
            at += width * 4;
        }
    }

    variable_byte(bytes, at, rows - packed, &mut out)?;
    Ok(out)
}

/// Unpacks one miniblock of thirty two values held at `width` bits each.
///
/// The packed form is a bit stream running least significant bit first through `width` little
/// endian words, so value `i` is the `width` bits starting at bit `i * width`, and a value that
/// reaches the end of a word carries on in the low bits of the next one. A width of zero writes no
/// words at all and means thirty two zeroes.
fn unpack(bytes: &[u8], at: usize, width: usize, out: &mut Vec<u32>) -> Result<()> {
    if width == 0 {
        out.resize(out.len() + MINI, 0);
        return Ok(());
    }

    let mut words = Vec::with_capacity(width);
    for word in 0..width {
        words.push(read_u32(bytes, at + word * 4, "a bit packed miniblock")?);
    }

    let mask = if width == 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    for value in 0..MINI {
        let start = value * width;
        let taken = 32 - start % 32;
        let low = words[start / 32] >> (start % 32);
        // Only when the value really does cross a word boundary. A value that starts on one takes
        // the whole word, so `taken` is thirty two and this is not reached, which is what keeps the
        // shift below the width of the type.
        let high = if taken < width {
            words[start / 32 + 1] << taken
        } else {
            0
        };
        out.push((low | high) & mask);
    }
    Ok(())
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
    fn pack(values: &[u32], width: usize) -> Vec<u8> {
        if width == 0 {
            return Vec::new();
        }
        let mut words = vec![0u32; width];
        for (index, value) in values.iter().enumerate() {
            let start = index * width;
            words[start / 32] |= value << (start % 32);
            let taken = 32 - start % 32;
            if taken < width {
                words[start / 32 + 1] |= value >> taken;
            }
        }
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
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
