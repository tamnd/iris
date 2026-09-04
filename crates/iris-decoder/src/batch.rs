//! Building a batch of rows to hand back.

use iris_abi::{BufferRef, Buffers, Nodes, Writer};

use crate::error::{Error, Result};

/// How large an encoded record is allowed to get before the decoder gives up on it.
///
/// This is not a limit on how much data a scan can return, because the data itself is not in the
/// record. It is a limit on how many arrays and buffers one batch can describe, and sixteen bytes
/// each means a megabyte covers thirty thousand of them. A batch that needs more than that is a bug
/// somewhere upstream, and the useful thing to do with a bug is stop rather than allocate.
const RECORD_LIMIT: usize = 1 << 20;

/// A batch of decoded rows, laid out the way Arrow wants them.
///
/// A decoder fills one of these in schema pre-order: for each array, one call to [`Batch::array`]
/// saying how long it is and how many nulls it has, then one call to [`Batch::buffer`] for each
/// buffer that array needs. Arrow's rules about which buffers an array has are not restated here
/// and are not enforced here. The host knows the schema, so the host is the side that can check the
/// shape, and duplicating that check in the guest would mean two implementations of it that can
/// disagree.
///
/// A buffer with no bytes is still a buffer. A fixed width array with no nulls has an absent
/// validity buffer, and absent means `buffer(&[])` rather than a missing call, because the position
/// in the list is what identifies it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Batch {
    rows: u64,
    nodes: Vec<u8>,
    buffers: Vec<Vec<u8>>,
}

impl Batch {
    /// An empty batch that will describe `rows` rows.
    #[must_use]
    pub const fn new(rows: u64) -> Self {
        Self {
            rows,
            nodes: Vec::new(),
            buffers: Vec::new(),
        }
    }

    /// Empties the batch and points it at a new row count, keeping the memory it already has.
    ///
    /// A scan that returns a thousand batches should not allocate a thousand times.
    pub fn reset(&mut self, rows: u64) {
        self.rows = rows;
        self.nodes.clear();
        for buffer in &mut self.buffers {
            buffer.clear();
        }
        // The outer vector keeps its inner vectors so their capacity survives, but it has to shrink
        // to nothing so the next batch starts pushing at index zero. Reuse across batches is a
        // separate exercise and it is not worth the bookkeeping until a real decoder asks for it.
        self.buffers.clear();
    }

    /// How many rows the batch describes.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// Adds one array, in schema pre-order.
    pub fn array(&mut self, length: u64, null_count: u64) -> &mut Self {
        self.nodes.extend_from_slice(&length.to_le_bytes());
        self.nodes.extend_from_slice(&null_count.to_le_bytes());
        self
    }

    /// Adds one Arrow buffer by copying it.
    pub fn buffer(&mut self, bytes: &[u8]) -> &mut Self {
        self.buffers.push(bytes.to_vec());
        self
    }

    /// Adds one Arrow buffer by writing it in place, for a decoder that would rather not build the
    /// bytes somewhere else first and then copy them here.
    pub fn buffer_with(&mut self, fill: impl FnOnce(&mut Vec<u8>)) -> &mut Self {
        let mut buffer = Vec::new();
        fill(&mut buffer);
        self.buffers.push(buffer);
        self
    }

    /// The arrays described so far.
    #[must_use]
    pub fn nodes(&self) -> Nodes<'_> {
        // The bytes were written sixteen at a time by `array`, so the length is a whole number of
        // nodes by construction and the error arm cannot be reached.
        Nodes::from_bytes(&self.nodes).unwrap_or(Nodes::EMPTY)
    }

    /// The buffers, in the order they were added.
    pub fn buffers(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        self.buffers.iter().map(Vec::as_slice)
    }

    /// Whether the batch describes no arrays at all, which is how a decoder says a scan is finished.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Encodes the batch as an ABI record, pointing at the buffers where they sit in memory.
    ///
    /// The offsets are addresses in this process. Inside a WebAssembly guest that is what the host
    /// wants, because the guest's whole address space is a byte array the host can index. Outside
    /// one it is only meaningful to code in the same process, which is why the buffers themselves
    /// stay borrowed from the batch and are not copied into the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::resource_limit`] if the batch describes more arrays and buffers than a
    /// record is allowed to hold.
    pub fn record(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut refs = Vec::with_capacity(self.buffers.len() * BufferRef::SIZE);
        for buffer in &self.buffers {
            // Casting a pointer to an integer is safe. Reading the bytes back at that address is
            // not, and that is the host's problem rather than this crate's, which is exactly the
            // split the design is trying to keep.
            let offset = buffer.as_ptr() as usize as u64;
            let len = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
            refs.extend_from_slice(&offset.to_le_bytes());
            refs.extend_from_slice(&len.to_le_bytes());
        }
        let buffers = Buffers::from_bytes(&refs)?;

        encode_into(out, |w| {
            iris_abi::Batch {
                rows: self.rows,
                flags: 0,
                nodes: self.nodes(),
                buffers,
            }
            .encode(w)
        })
    }
}

/// Writes a record into a growable buffer, doubling until it fits.
///
/// The alternative is a dry run that measures first, which means every record's layout is written
/// out twice and the two copies can drift. Doubling costs a repeated encode on the rare occasion the
/// first guess is short, and it cannot drift because there is only one implementation.
pub(crate) fn encode_into(
    out: &mut Vec<u8>,
    body: impl Fn(&mut Writer<'_>) -> iris_abi::Result<()>,
) -> Result<()> {
    let mut room = out.capacity().max(256);
    loop {
        out.clear();
        out.resize(room, 0);
        let mut w = Writer::new(out);
        match body(&mut w) {
            Ok(()) => {
                let written = w.position();
                out.truncate(written);
                return Ok(());
            }
            Err(iris_abi::Error::BufferFull { .. }) if room < RECORD_LIMIT => room *= 2,
            Err(iris_abi::Error::BufferFull { .. }) => {
                out.clear();
                return Err(Error::resource_limit(
                    "the decoder's answer is larger than a single record is allowed to be",
                ));
            }
            Err(err) => {
                out.clear();
                return Err(err.into());
            }
        }
    }
}
