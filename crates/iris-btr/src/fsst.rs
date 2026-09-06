//! The `FSST` symbol table the reference compresses strings with.
//!
//! `FSST` is Fast Static Symbol Table compression, from the paper by Boncz, Neumann and Leis, and it
//! is a separate library rather than something `BtrBlocks` wrote. The reference links against
//! github.com/cwida/fsst, hands it the strings, and stores what came back verbatim, so the bytes in
//! an `FSST` chunk are that library's format. This module reads it.
//!
//! The idea is one byte a code and up to eight bytes a symbol, with a table of at most two hundred
//! and fifty five symbols shared by the whole column. Code two hundred and fifty five is not a
//! symbol, it is an escape saying the next byte is a literal, which is how a byte no symbol covers
//! gets through. That is the whole format on the reading side: there is no entropy coding under it
//! and no state carried between codes, so decompressing is a walk.
//!
//! # The table is a histogram rather than a list
//!
//! A symbol table is not stored as a list of symbols with their lengths. It is eight counts, one a
//! length, and then the symbols' bytes run together with nothing between them. A reader gets the
//! lengths back by walking the counts in the order the writer used, which is lengths two through
//! eight and then length one last. Reading them in the obvious order of one through eight would
//! come back with a table that is the right size and wrong throughout.
//!
//! # What the reference does with a code that is not in the table
//!
//! It fills the unused codes with the word `corrupt` and decodes them, which is a debugging aid
//! rather than an answer. This refuses instead, the same way a dictionary code outside the
//! dictionary is refused, because a part that uses a code its own table does not define is making a
//! claim about itself that is not true.

use crate::error::{Error, Result};

/// The version the reference's copy of the library writes and reads.
const VERSION: u32 = 20_190_218;

/// The code that means the next byte is a literal rather than a symbol.
const ESCAPE: u8 = 255;

/// How many codes there are that are not the escape.
const CODES: usize = 255;

/// The longest a symbol can be.
const SYMBOL: usize = 8;

/// How many bytes the header takes before the symbols start.
const HEADER: usize = 17;

/// How many bytes the reference reserves for a serialised table however long it really is.
///
/// The library's own maximum, which is the header, every symbol at its longest, and one byte over.
/// The reference always leaves this much room and records where the strings start anyway, so this is
/// here to be checked against rather than to be counted on.
pub(crate) const MAX_HEADER: usize = HEADER + CODES * SYMBOL + 1;

/// A symbol table, as one entry a code.
#[derive(Debug)]
pub(crate) struct Table {
    /// Each code's symbol, in the low `lengths[code]` bytes.
    symbols: [[u8; SYMBOL]; CODES],
    /// How many of each symbol's bytes count.
    lengths: [u8; CODES],
    /// How many codes the table defines. The rest are not symbols and are refused.
    defined: usize,
}

/// Reads a serialised symbol table.
///
/// Eight bytes of version, a flag byte, eight counts, and then the symbols' bytes. The count at
/// index `n` is how many symbols there are of length `n + 1` for `n` in one through seven, and the
/// one at index zero is how many there are of length one, which is why it is read last.
pub(crate) fn read(bytes: &[u8]) -> Result<Table> {
    // The version sits in the high half of the first word, with the low half holding three fields a
    // writer would need to rebuild an encoder. None of those are needed to read, so none are read.
    let version = word(bytes, 4)?;
    if version != VERSION {
        return Err(Error::Malformed {
            what: "a symbol table",
            why: "it is not a version of FSST the reference reads",
        });
    }

    let zero_terminated = byte(bytes, 8)? & 1 != 0;
    let mut counts = [0u8; SYMBOL];
    counts.copy_from_slice(bytes.get(9..HEADER).ok_or(Error::Truncated {
        what: "a symbol table",
        from: 9,
        to: HEADER,
        len: bytes.len(),
    })?);

    let mut symbols = [[0u8; SYMBOL]; CODES];
    let mut lengths = [0u8; CODES];

    // A zero terminated table has the empty string as code zero, and does not store it, so it is
    // written here and the count that would have covered it is taken off. The reference writes this
    // one before it knows whether the table is zero terminated and lets the loop below overwrite it
    // when it is not, which comes to the same thing.
    lengths[0] = 1;
    let mut defined = usize::from(zero_terminated);
    if zero_terminated {
        counts[0] = counts[0].checked_sub(1).ok_or(Error::Malformed {
            what: "a symbol table",
            why: "it is zero terminated and holds no symbol of one byte",
        })?;
    }

    let mut at = HEADER;
    for step in 1..=8u8 {
        // One through seven give lengths two through eight, and then eight comes back round to the
        // count at index zero, which is the symbols of one byte.
        let slot = step & 7;
        let length = usize::from(slot) + 1;
        for _ in 0..counts[usize::from(slot)] {
            if defined >= CODES {
                return Err(Error::Malformed {
                    what: "a symbol table",
                    why: "it holds more symbols than there are codes for them",
                });
            }
            let symbol =
                bytes
                    .get(at..)
                    .and_then(|rest| rest.get(..length))
                    .ok_or(Error::Truncated {
                        what: "a symbol table",
                        from: at,
                        to: at + length,
                        len: bytes.len(),
                    })?;
            symbols[defined][..length].copy_from_slice(symbol);
            lengths[defined] = slot + 1;
            defined += 1;
            at += length;
        }
    }

    Ok(Table {
        symbols,
        lengths,
        defined,
    })
}

/// Decompresses a run of codes onto the end of `out`.
///
/// A code is either the escape, in which case the byte after it is a literal, or a symbol, in which
/// case its bytes go out as they are. The reference has three versions of this loop, one reading
/// four codes at a time and one writing eight bytes at a time and this one, and they agree on every
/// answer. The wide ones exist because they can write past the end of a symbol and fix it up
/// afterwards, which is worth doing when the buffer is yours and not worth doing here.
pub(crate) fn decompress(table: &Table, codes: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let mut at = 0;
    while at < codes.len() {
        let code = codes[at];
        at += 1;
        if code == ESCAPE {
            let literal = codes.get(at).ok_or(Error::Truncated {
                what: "a run of FSST codes",
                from: at,
                to: at + 1,
                len: codes.len(),
            })?;
            out.push(*literal);
            at += 1;
        } else {
            let code = usize::from(code);
            if code >= table.defined {
                return Err(Error::Malformed {
                    what: "a run of FSST codes",
                    why: "it uses a code its own symbol table does not define",
                });
            }
            let length = usize::from(table.lengths[code]);
            out.extend_from_slice(&table.symbols[code][..length]);
        }
    }
    Ok(())
}

/// Reads one byte of a table's header.
fn byte(bytes: &[u8], at: usize) -> Result<u8> {
    bytes.get(at).copied().ok_or(Error::Truncated {
        what: "a symbol table",
        from: at,
        to: at + 1,
        len: bytes.len(),
    })
}

/// Reads one little endian word of a table's header.
fn word(bytes: &[u8], at: usize) -> Result<u32> {
    let word = bytes
        .get(at..)
        .and_then(|rest| rest.get(..4))
        .ok_or(Error::Truncated {
            what: "a symbol table",
            from: at,
            to: at + 4,
            len: bytes.len(),
        })?;
    Ok(u32::from_le_bytes(word.try_into().unwrap_or([0; 4])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises a symbol table the way the library's own writer does.
    fn table(symbols: &[&[u8]]) -> Vec<u8> {
        // The writer emits the symbols in code order and the counts say what their lengths were, so
        // a table is only well formed if the symbols are already grouped by length in the order the
        // reader walks them. Sorting here is what makes a test able to write them any way round.
        let mut sorted: Vec<&[u8]> = symbols.to_vec();
        sorted.sort_by_key(|symbol| (symbol.len() + 6) % SYMBOL);

        let mut counts = [0u8; SYMBOL];
        for symbol in &sorted {
            counts[(symbol.len() - 1) % SYMBOL] += 1;
        }

        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&counts);
        for symbol in &sorted {
            bytes.extend_from_slice(symbol);
        }
        bytes
    }

    /// Decompresses `codes` against a table of `symbols`.
    fn decode(symbols: &[&[u8]], codes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        decompress(&read(&table(symbols))?, codes, &mut out)?;
        Ok(out)
    }

    /// The same, for a case that is meant to work.
    fn decoded(symbols: &[&[u8]], codes: &[u8]) -> Vec<u8> {
        decode(symbols, codes).expect("a run of codes")
    }

    #[test]
    fn symbols_are_read_back_in_the_order_the_writer_emitted_them() {
        // The counts are walked as lengths two through eight and then one, so these five symbols are
        // stored in the order `bb`, `ccc`, `dddd`, `a`, `e` and get codes nought through four in
        // that order. A reader that walked the counts as one through eight would hand back the same
        // five symbols under different codes, and every code in a stream would name the wrong one.
        let symbols: [&[u8]; 5] = [b"a", b"bb", b"ccc", b"dddd", b"e"];
        assert_eq!(decoded(&symbols, &[0, 1, 2, 3, 4]), b"bbcccddddae".to_vec());
    }

    #[test]
    fn a_run_of_codes_comes_out_as_the_symbols_run_together() {
        assert_eq!(
            decoded(&[b"the ", b"quick", b" "], &[0, 1, 2, 0, 1]),
            b"the quick the quick".to_vec()
        );
    }

    #[test]
    fn the_escape_puts_the_byte_after_it_through_untouched() {
        // Including an escaped two hundred and fifty five, which is the byte that would otherwise be
        // read as another escape.
        assert_eq!(
            decoded(&[b"ab"], &[0, 255, b'z', 0, 255, 255]),
            vec![b'a', b'b', b'z', b'a', b'b', 255]
        );
    }

    #[test]
    fn a_symbol_of_the_longest_length_is_read_whole() {
        assert_eq!(
            decoded(&[b"12345678", b"x"], &[0, 1, 0]),
            b"12345678x12345678".to_vec()
        );
    }

    #[test]
    fn a_code_the_table_does_not_define_is_refused() {
        // The reference fills the unused codes with the word `corrupt` and decodes them, so a part
        // like this comes back with an answer there rather than an error.
        assert!(matches!(
            decode(&[b"ab", b"cd"], &[0, 7]),
            Err(Error::Malformed {
                what: "a run of FSST codes",
                ..
            })
        ));
    }

    #[test]
    fn an_escape_with_nothing_after_it_is_refused() {
        assert!(matches!(
            decode(&[b"ab"], &[0, 255]),
            Err(Error::Truncated {
                what: "a run of FSST codes",
                ..
            })
        ));
    }

    #[test]
    fn a_table_of_another_version_is_refused() {
        let mut bytes = table(&[b"ab"]);
        bytes[4..8].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert!(matches!(
            read(&bytes),
            Err(Error::Malformed {
                what: "a symbol table",
                ..
            })
        ));
    }

    #[test]
    fn a_table_that_ends_in_the_middle_of_a_symbol_is_refused() {
        let mut bytes = table(&[b"abcdefgh"]);
        bytes.pop();
        assert!(matches!(
            read(&bytes),
            Err(Error::Truncated {
                what: "a symbol table",
                ..
            })
        ));
    }

    #[test]
    fn a_table_claiming_more_symbols_than_there_are_codes_allocates_nothing() {
        // Two hundred and fifty five symbols of every length, which is more than eight times what a
        // code can name. The reference writes each of them into a fixed array as it goes.
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[255; SYMBOL]);
        bytes.resize(bytes.len() + 16_384, b'x');
        assert!(matches!(
            read(&bytes),
            Err(Error::Malformed {
                what: "a symbol table",
                ..
            })
        ));
    }

    #[test]
    fn a_zero_terminated_table_holds_the_empty_symbol_as_code_zero() {
        // Nothing the reference writes here sets this flag, but the format has it, and a reader that
        // ignored it would take the first stored symbol as code zero and be one out from there on.
        // The count of one byte symbols covers the empty one, so setting the flag on a table of
        // `a` and `bb` leaves code zero as the empty string, code one as `bb`, and the byte that
        // held `a` unread. Without the flag those same bytes are code zero `bb` and code one `a`.
        let mut bytes = table(&[b"a", b"bb"]);
        bytes[8] = 1;
        let mut out = Vec::new();
        decompress(&read(&bytes).expect("a table"), &[0, 1], &mut out).expect("a run");
        assert_eq!(out, b"\0bb".to_vec());

        bytes[8] = 0;
        let mut out = Vec::new();
        decompress(&read(&bytes).expect("a table"), &[0, 1], &mut out).expect("a run");
        assert_eq!(out, b"bba".to_vec());
    }

    #[test]
    fn a_zero_terminated_table_with_no_symbol_of_one_byte_is_refused() {
        let mut bytes = table(&[b"bb"]);
        bytes[8] = 1;
        assert!(matches!(
            read(&bytes),
            Err(Error::Malformed {
                what: "a symbol table",
                ..
            })
        ));
    }
}
