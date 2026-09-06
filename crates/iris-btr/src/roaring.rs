//! The Roaring bitmap a scattered nullmap is stored as.
//!
//! The reference does not write a nullmap of its own once the rows stop agreeing with each other.
//! It builds a `CRoaring` bitmap of whichever rows are the exception and asks that library to
//! serialise it, so the bytes in the chunk are `CRoaring`'s and this module reads them.
//!
//! # It is the format the library calls non portable
//!
//! `CRoaring` has two serialisations. The portable one is the format written down in the Roaring
//! specification and shared with the Java and Go implementations. The other is what
//! `roaring_bitmap_serialize` writes, and the reference asks for that one by passing `false` where
//! the library's default is `true`. It is a tag byte and then one of two things: the portable format
//! after all, or, when a plain sorted list of the values would take fewer bytes, a count and that
//! list. The choice is made on size alone, so both arms turn up in the same corpus and neither is a
//! fallback for the other.
//!
//! # Run containers are always a possibility
//!
//! Before writing, the reference calls `runOptimize`, which converts any container that would be
//! smaller as a list of runs. A reader that handled only the array and bitset containers would work
//! until it met a column whose nulls happen to be consecutive, which is exactly the shape a real
//! column tends to have. All three container kinds are read here.
//!
//! What this hands back is one entry a row saying whether the set holds it. Whether the set holds
//! the present rows or the null ones is the caller's business, since that is decided by the encoding
//! byte in the chunk header and by nothing in these bytes.

use crate::error::{Error, Result};
use crate::part::read_u32;

/// The tag saying the rest is a count and then the values themselves.
const ARRAY: u8 = 1;

/// The tag saying the rest is the portable format.
const CONTAINER: u8 = 2;

/// The cookie of a portable bitmap that has run containers in it.
///
/// Only the low half of the word is the cookie. The high half holds one less than the number of
/// containers, which is why this form does not write the count separately.
const COOKIE_WITH_RUNS: u32 = 12347;

/// The cookie of a portable bitmap that has none, which is followed by the container count.
const COOKIE_NO_RUNS: u32 = 12346;

/// How few containers a bitmap can have and still be written without the offset header.
///
/// That header is a byte offset per container and it exists to let a reader jump to one without
/// walking the ones before it. Below this many the writer decides it is not worth the bytes, and
/// only in the form that has run containers. This reader walks the containers in order either way,
/// so the header is skipped rather than used, but it has to be skipped by the right amount.
const NO_OFFSET_THRESHOLD: usize = 4;

/// The largest cardinality still written as a list of values rather than as a bitset.
const ARRAY_MAX: u32 = 4096;

/// How many bytes a bitset container occupies, which is a bit for each value a container covers.
const BITSET_BYTES: usize = 8192;

/// How many containers a bitmap can have, one for each high half of a value.
const MAX_CONTAINERS: usize = 1 << 16;

/// Reads a serialised bitmap and says, for each of `rows` rows, whether the set holds it.
///
/// A value outside the rows the chunk says it has is refused rather than dropped. The reference
/// would set that position in a bitset sized from the row count, which is a write past the end of an
/// allocation, so a part carrying one is a part the reference itself could not read safely and there
/// is no answer of ours to compare against.
pub(crate) fn read(bytes: &[u8], rows: usize) -> Result<Vec<bool>> {
    // Sized from the row count in the chunk header and never from anything in these bytes, so a
    // bitmap claiming a great many containers costs time to refuse and no memory.
    let mut set = vec![false; rows];
    match bytes.first().copied() {
        Some(ARRAY) => values(bytes, &mut set)?,
        Some(CONTAINER) => portable(&bytes[1..], &mut set)?,
        Some(_) => {
            return Err(Error::Malformed {
                what: "a roaring bitmap",
                why: "it starts with a tag the library does not write",
            });
        }
        None => {
            return Err(Error::Truncated {
                what: "a roaring bitmap",
                from: 0,
                to: 1,
                len: bytes.len(),
            });
        }
    }
    Ok(set)
}

/// Reads the arm that is a count and then the values, one word each, in order.
fn values(bytes: &[u8], set: &mut [bool]) -> Result<()> {
    let count = usize::try_from(read_u32(bytes, 1, "a roaring value list")?).unwrap_or(usize::MAX);
    for index in 0..count {
        let at = index.saturating_mul(4).saturating_add(5);
        mark(read_u32(bytes, at, "a roaring value list")?, set)?;
    }
    Ok(())
}

/// Reads the portable format, which is a header describing the containers and then the containers.
fn portable(bytes: &[u8], set: &mut [bool]) -> Result<()> {
    let cookie = read_u32(bytes, 0, "a roaring bitmap")?;
    let runs = cookie & 0xffff == COOKIE_WITH_RUNS;
    let (containers, mut at) = if runs {
        // One less than the count, held in the half of the word the cookie does not use.
        let described = usize::try_from(cookie >> 16).unwrap_or(MAX_CONTAINERS);
        (described + 1, 4)
    } else if cookie == COOKIE_NO_RUNS {
        let described = read_u32(bytes, 4, "a roaring bitmap")?;
        (usize::try_from(described).unwrap_or(usize::MAX), 8)
    } else {
        return Err(Error::Malformed {
            what: "a roaring bitmap",
            why: "it does not start with either of the cookies the library writes",
        });
    };

    if containers > MAX_CONTAINERS {
        return Err(Error::Malformed {
            what: "a roaring bitmap",
            why: "it claims more containers than there are keys for one to have",
        });
    }

    // Which containers are run containers, a bit each, and only when the cookie said there are any.
    let flags = at;
    if runs {
        at += containers.div_ceil(8);
    }

    // A key and a cardinality for every container, all of them before any container's bytes.
    let described = at;
    at += containers * 4;

    if !runs || containers >= NO_OFFSET_THRESHOLD {
        at += containers * 4;
    }

    for index in 0..containers {
        let entry = described + index * 4;
        let key = read_u16(bytes, entry, "a roaring container")?;
        // Written as one less than the real cardinality, because a container holding every one of
        // the values it covers would not otherwise fit in the sixteen bits it is given.
        let cardinality = u32::from(read_u16(bytes, entry + 2, "a roaring container")?) + 1;
        let base = u32::from(key) << 16;

        at += if runs && flag(bytes, flags, index)? {
            run_container(bytes, at, base, set)?
        } else if cardinality > ARRAY_MAX {
            bitset_container(bytes, at, base, set)?
        } else {
            array_container(bytes, at, cardinality, base, set)?
        };
    }
    Ok(())
}

/// Whether the container at `index` is a run container.
fn flag(bytes: &[u8], flags: usize, index: usize) -> Result<bool> {
    let at = flags.saturating_add(index / 8);
    let byte = *bytes.get(at).ok_or(Error::Truncated {
        what: "the run container flags",
        from: flags,
        to: at + 1,
        len: bytes.len(),
    })?;
    Ok(byte & (1 << (index % 8)) != 0)
}

/// Reads a container that lists its values, and says how many bytes it took.
fn array_container(
    bytes: &[u8],
    at: usize,
    cardinality: u32,
    base: u32,
    set: &mut [bool],
) -> Result<usize> {
    let cardinality = usize::try_from(cardinality).unwrap_or(usize::MAX);
    for index in 0..cardinality {
        let value = read_u16(
            bytes,
            at.saturating_add(index * 2),
            "a roaring array container",
        )?;
        mark(base | u32::from(value), set)?;
    }
    Ok(cardinality * 2)
}

/// Reads a container that is a bitset over everything it covers, and says how many bytes it took.
fn bitset_container(bytes: &[u8], at: usize, base: u32, set: &mut [bool]) -> Result<usize> {
    let to = at.saturating_add(BITSET_BYTES);
    let words = bytes
        .get(at..to)
        .ok_or(Error::Truncated {
            what: "a roaring bitset container",
            from: at,
            to,
            len: bytes.len(),
        })?
        .as_chunks::<8>()
        .0;

    // Counted alongside the words rather than derived from an index, which keeps the arithmetic in
    // the width the values are in and needs no cast to get there.
    let mut low = 0u32;
    for word in words.iter().copied().map(u64::from_le_bytes) {
        let mut word = word;
        while word != 0 {
            mark(base | (low + word.trailing_zeros()), set)?;
            // Clears the lowest set bit, so this runs once for each value the word holds rather
            // than sixty four times whatever the word is.
            word &= word - 1;
        }
        low += 64;
    }
    Ok(BITSET_BYTES)
}

/// Reads a container that is a list of runs, and says how many bytes it took.
///
/// A run is a start and a length, and the length is one less than the number of values in it, so
/// every run holds at least one value and a run covering the whole container still fits in sixteen
/// bits. The cardinality in the header is not used, because the reference does not use it either,
/// and refusing a container where the two disagree would refuse a part the reference reads.
fn run_container(bytes: &[u8], at: usize, base: u32, set: &mut [bool]) -> Result<usize> {
    let runs = usize::from(read_u16(bytes, at, "a roaring run container")?);
    for index in 0..runs {
        let entry = at.saturating_add(2 + index * 4);
        let start = u32::from(read_u16(bytes, entry, "a roaring run")?);
        let length = u32::from(read_u16(bytes, entry + 2, "a roaring run")?);
        for value in start..=start + length {
            mark(base | value, set)?;
        }
    }
    Ok(2 + runs * 4)
}

/// Records that the set holds `value`.
fn mark(value: u32, set: &mut [bool]) -> Result<()> {
    let row = usize::try_from(value)
        .ok()
        .filter(|row| *row < set.len())
        .ok_or(Error::Malformed {
            what: "a roaring bitmap",
            why: "it holds a row past the end of the chunk",
        })?;
    set[row] = true;
    Ok(())
}

/// Reads a sixteen bit field, the way [`read_u32`] reads a thirty two bit one.
fn read_u16(bytes: &[u8], at: usize, what: &'static str) -> Result<u16> {
    let to = at.checked_add(2).ok_or(Error::Truncated {
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
    Ok(u16::from_le_bytes([field[0], field[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portable header for a bitmap whose containers are all described the same way.
    ///
    /// `keys` is a key and a true cardinality a container, and `kinds` says which of them are run
    /// containers, which is what decides both the cookie and whether the flag bytes are there.
    fn header(keys: &[(u16, u16)], kinds: &[bool]) -> Vec<u8> {
        let runs = kinds.iter().any(|kind| *kind);
        let mut bytes = vec![CONTAINER];
        if runs {
            let count = u32::try_from(keys.len() - 1).expect("a container count");
            bytes.extend_from_slice(&(COOKIE_WITH_RUNS | (count << 16)).to_le_bytes());
            let mut flags = vec![0u8; keys.len().div_ceil(8)];
            for (index, kind) in kinds.iter().enumerate() {
                if *kind {
                    flags[index / 8] |= 1 << (index % 8);
                }
            }
            bytes.extend_from_slice(&flags);
        } else {
            bytes.extend_from_slice(&COOKIE_NO_RUNS.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(keys.len())
                    .expect("a container count")
                    .to_le_bytes(),
            );
        }
        for (key, cardinality) in keys {
            bytes.extend_from_slice(&key.to_le_bytes());
            // Written one short, the way the format writes it.
            bytes.extend_from_slice(&(cardinality - 1).to_le_bytes());
        }
        // The offsets, which this reader skips, so what is in them only has to be the right length.
        if !runs || keys.len() >= NO_OFFSET_THRESHOLD {
            bytes.extend_from_slice(&vec![0u8; keys.len() * 4]);
        }
        bytes
    }

    /// The rows a set holds, which is easier to compare against than a vector of flags.
    fn rows(set: &[bool]) -> Vec<u32> {
        set.iter()
            .enumerate()
            .filter(|(_, held)| **held)
            .map(|(row, _)| u32::try_from(row).expect("a row"))
            .collect()
    }

    #[test]
    fn a_plain_list_of_values_is_read() {
        // The arm the library picks when the portable form would be the larger of the two.
        let mut bytes = vec![ARRAY];
        bytes.extend_from_slice(&3u32.to_le_bytes());
        for value in [2u32, 9, 40] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(rows(&read(&bytes, 64).expect("a bitmap")), vec![2, 9, 40]);
    }

    #[test]
    fn an_array_container_is_read() {
        let mut bytes = header(&[(0, 2)], &[false]);
        for value in [7u16, 300] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(rows(&read(&bytes, 512).expect("a bitmap")), vec![7, 300]);
    }

    #[test]
    fn a_run_container_is_read() {
        // Two runs, and the length is one less than the values in it, so this is 4, 5, 6 and 9.
        let mut bytes = header(&[(0, 4)], &[true]);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        for (start, length) in [(4u16, 2u16), (9, 0)] {
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
        }
        assert_eq!(rows(&read(&bytes, 16).expect("a bitmap")), vec![4, 5, 6, 9]);
    }

    #[test]
    fn a_bitset_container_is_read() {
        // Anything past four thousand and ninety six values is a bitset rather than a list, and the
        // cardinality in the header is what says so.
        let held: Vec<u32> = (0..5000).map(|value| value * 2).collect();
        let mut words = vec![0u64; BITSET_BYTES / 8];
        for value in &held {
            words[*value as usize / 64] |= 1 << (value % 64);
        }
        let mut bytes = header(&[(0, 5000)], &[false]);
        bytes.extend(words.iter().flat_map(|word| word.to_le_bytes()));
        assert_eq!(rows(&read(&bytes, 10_000).expect("a bitmap")), held);
    }

    #[test]
    fn containers_are_read_in_order_and_their_keys_are_the_high_half() {
        // Three containers of different kinds in one bitmap, which is what checks that each one
        // reports the right length for the next to start after.
        let mut bytes = header(&[(0, 1), (1, 2), (5, 1)], &[false, true, false]);
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            rows(&read(&bytes, 1 << 20).expect("a bitmap")),
            vec![3, (1 << 16) + 8, (1 << 16) + 9, (5 << 16) + 2]
        );
    }

    #[test]
    fn a_bitmap_holding_a_row_the_chunk_does_not_have_is_refused() {
        let mut bytes = vec![ARRAY];
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            read(&bytes, 8),
            Err(Error::Malformed {
                what: "a roaring bitmap",
                ..
            })
        ));
    }

    #[test]
    fn a_bitmap_with_neither_cookie_is_refused() {
        let mut bytes = vec![CONTAINER];
        bytes.extend_from_slice(&1234u32.to_le_bytes());
        assert!(matches!(
            read(&bytes, 8),
            Err(Error::Malformed {
                what: "a roaring bitmap",
                ..
            })
        ));
    }

    #[test]
    fn a_bitmap_with_a_tag_the_library_does_not_write_is_refused() {
        assert!(matches!(
            read(&[7], 8),
            Err(Error::Malformed {
                what: "a roaring bitmap",
                ..
            })
        ));
    }

    #[test]
    fn a_value_list_longer_than_the_bytes_there_allocates_nothing() {
        // A count of four billion against nine bytes. Sizing anything from it would be the bug.
        let mut bytes = vec![ARRAY];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(read(&bytes, 8), Err(Error::Truncated { .. })));
    }

    #[test]
    fn a_container_the_bytes_run_out_before_is_refused() {
        let bytes = header(&[(0, 9)], &[false]);
        assert!(matches!(read(&bytes, 64), Err(Error::Truncated { .. })));
    }
}
