//! Reading and writing the primitive values that ABI records are built out of.
//!
//! Everything on the wire is little-endian, because every machine anyone is going to run this on
//! is little-endian and pretending otherwise would cost real instructions in the hot path for a
//! portability nobody is asking for. If that ever stops being true it is a new ABI major version,
//! not a runtime flag.
//!
//! Variable-length fields are padded so that the cursor lands back on a multiple of eight. That
//! costs at most seven bytes per field and it means a later version of this crate can read a fixed
//! width run of a record by pointing at it instead of copying it out field by field.

use crate::error::{Error, Result};

/// How wide the alignment of a record payload is, in bytes.
///
/// Record headers are exactly this wide too, so a record that starts aligned has a payload that
/// starts aligned.
pub const ALIGN: usize = 8;

/// Rounds `n` up to the next multiple of [`ALIGN`].
#[must_use]
pub const fn align_up(n: usize) -> usize {
    n.next_multiple_of(ALIGN)
}

/// A cursor that reads ABI values out of a byte slice.
///
/// The reader borrows its input and hands back borrowed slices, so decoding a record does not
/// allocate and does not copy the payload.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts reading at the beginning of `buf`.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// How many bytes are left.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the reader has consumed everything.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// How far into the buffer the cursor is.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let available = self.remaining();
        if n > available {
            return Err(Error::Truncated {
                needed: n,
                available,
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads one byte.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer is exhausted.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than two bytes are left.
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Reads a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than four bytes are left.
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than eight bytes are left.
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Reads a `u64` that a later version of a record appended, if the writer knew about it.
    ///
    /// This is the other half of the grow-at-the-end rule. A reader that does not care about a new
    /// field just stops early and the framing puts it on the next record. A reader that does care
    /// has to tell two situations apart: a writer that predates the field, which is fine and means
    /// the field is absent, and a payload that was cut in half, which is not fine. Nothing left is
    /// the first, something but not enough is the second.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if there is at least one byte left but fewer than eight.
    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        if self.is_empty() {
            return Ok(None);
        }
        self.u64().map(Some)
    }

    /// Reads exactly `n` bytes and borrows them from the input.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than `n` bytes are left.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    /// Skips `n` bytes.
    ///
    /// This is how a reader gets past a field it does not understand, which is the whole reason the
    /// format carries lengths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than `n` bytes are left.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    /// Skips forward to the next alignment boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the padding runs off the end of the buffer.
    pub fn align(&mut self) -> Result<()> {
        self.skip(align_up(self.pos) - self.pos)
    }

    /// Reads a length-prefixed byte string, including its trailing padding.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer ends inside the field, or
    /// [`Error::LengthOverflow`] if the declared length does not fit in a `usize`.
    pub fn var_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()?;
        let len = usize::try_from(len).map_err(|_| Error::LengthOverflow)?;
        let out = self.take(len)?;
        self.align()?;
        Ok(out)
    }

    /// Reads a length-prefixed UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotUtf8`] if the bytes are not valid UTF-8, or the same errors as
    /// [`Reader::var_bytes`].
    pub fn var_str(&mut self) -> Result<&'a str> {
        core::str::from_utf8(self.var_bytes()?).map_err(|_| Error::NotUtf8)
    }

    /// Splits off a reader over the next `n` bytes and steps this one past them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than `n` bytes are left.
    pub fn sub(&mut self, n: usize) -> Result<Reader<'a>> {
        Ok(Reader::new(self.take(n)?))
    }
}

/// A cursor that writes ABI values into a byte slice.
///
/// The writer never grows its buffer. A caller that does not know how big a record will be should
/// size the buffer with [`Writer::position`] on a dry run, or just use a buffer big enough for the
/// largest record the ABI allows.
#[derive(Debug)]
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Starts writing at the beginning of `buf`.
    #[must_use]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// How many bytes have been written.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// How much room is left.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Everything written so far.
    #[must_use]
    pub fn written(&self) -> &[u8] {
        &self.buf[..self.pos]
    }

    fn room(&mut self, n: usize) -> Result<usize> {
        let available = self.remaining();
        if n > available {
            return Err(Error::BufferFull {
                needed: n,
                available,
            });
        }
        let at = self.pos;
        self.pos += n;
        Ok(at)
    }

    /// Writes one byte.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn u8(&mut self, v: u8) -> Result<()> {
        let at = self.room(1)?;
        self.buf[at] = v;
        Ok(())
    }

    /// Writes a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn u16(&mut self, v: u16) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }

    /// Writes a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn u32(&mut self, v: u32) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }

    /// Writes a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn u64(&mut self, v: u64) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }

    /// Writes bytes with no length prefix and no padding.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn raw(&mut self, v: &[u8]) -> Result<()> {
        let at = self.room(v.len())?;
        self.buf[at..at + v.len()].copy_from_slice(v);
        Ok(())
    }

    /// Writes zeroes up to the next alignment boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room.
    pub fn align(&mut self) -> Result<()> {
        let pad = align_up(self.pos) - self.pos;
        let at = self.room(pad)?;
        self.buf[at..at + pad].fill(0);
        Ok(())
    }

    /// Writes a length-prefixed byte string and pads it out.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if there is no room, or [`Error::LengthOverflow`] if the slice
    /// is longer than a `u32` can describe.
    pub fn var_bytes(&mut self, v: &[u8]) -> Result<()> {
        let len = u32::try_from(v.len()).map_err(|_| Error::LengthOverflow)?;
        self.u32(len)?;
        self.raw(v)?;
        self.align()
    }

    /// Writes a length-prefixed string.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Writer::var_bytes`].
    pub fn var_str(&mut self, v: &str) -> Result<()> {
        self.var_bytes(v.as_bytes())
    }

    /// Overwrites a `u32` that was already written, at absolute offset `at`.
    ///
    /// This exists so a record can reserve its length field, write its payload, and then fill the
    /// length in once it is known.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferFull`] if `at` is not inside what has been written.
    pub fn patch_u32(&mut self, at: usize, v: u32) -> Result<()> {
        if at + 4 > self.pos {
            return Err(Error::BufferFull {
                needed: at + 4,
                available: self.pos,
            });
        }
        self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        Ok(())
    }
}
