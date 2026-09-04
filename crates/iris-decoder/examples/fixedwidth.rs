//! The trivial decoder: fixed width integers, no compression, no nulls.
//!
//! This one is deliberately boring, because the thing being tested is the contract rather than the
//! decoding. It is also the decoder `iris-runtime` runs in its gate test, so it is the first piece
//! of code in the tree that goes all the way from a container on disk to an Arrow `RecordBatch`.
//!
//! # The layout it reads
//!
//! | Offset | Width | Meaning |
//! | --- | --- | --- |
//! | 0 | 8 | How many rows |
//! | 8 | 8 | How many columns |
//! | 16 | rows times columns times 8 | The values, column by column |
//!
//! Every column is a non-nullable little endian `i64`, laid out contiguously, one column after
//! another. There is no footer, no compression and no index, which is the point: a format this
//! simple has nowhere to hide a bug that is really an ABI bug.

use iris_abi::{Capability, CapabilitySet};
use iris_decoder::{Batch, Decoder, Error, Request, Result, Session, Sink, Source, export_decoder};

/// Two `u64` values, so the header is one range request.
const HEADER: u64 = 16;

/// The width of every value in this format.
const WIDTH: u64 = 8;

/// A reader for the layout above.
struct FixedWidth {
    rows: u64,
    columns: u64,
    batch_rows: u64,
}

impl FixedWidth {
    /// Where column `column` keeps row `row`.
    const fn offset(&self, column: u64, row: u64) -> u64 {
        HEADER + (column * self.rows + row) * WIDTH
    }
}

impl Decoder for FixedWidth {
    const NAME: &'static str = "fixedwidth";
    const REQUIRES: CapabilitySet = CapabilitySet::new().with(Capability::RANDOM_ACCESS);
    const OPTIONAL: CapabilitySet = CapabilitySet::new().with(Capability::PROJECTION);

    fn open(session: &Session, source: &mut dyn Source) -> Result<Self> {
        let header = source.range(0, HEADER)?;
        let rows = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
        let columns = u64::from_le_bytes(header[8..].try_into().expect("eight bytes"));

        // A header that describes more data than the source holds is the one thing worth checking
        // here, and checking it at open means a bad container fails before a query has started
        // rather than partway through the first batch somebody was waiting on.
        let declared = rows
            .checked_mul(columns)
            .and_then(|values| values.checked_mul(WIDTH))
            .and_then(|bytes| bytes.checked_add(HEADER))
            .ok_or_else(|| {
                Error::malformed("the header describes more bytes than exist anywhere")
            })?;
        if session.source_bytes() != 0 && declared > session.source_bytes() {
            return Err(Error::malformed(
                "the header describes more rows than the source has bytes for",
            ));
        }

        Ok(Self {
            rows,
            columns,
            batch_rows: session.max_batch_rows().max(1),
        })
    }

    fn scan(
        &mut self,
        request: &Request<'_>,
        source: &mut dyn Source,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        let columns: Vec<u64> = match request.columns() {
            Some(projected) => projected.map(u64::from).collect(),
            None => (0..self.columns).collect(),
        };
        if let Some(&out_of_range) = columns.iter().find(|&&c| c >= self.columns) {
            let _ = out_of_range;
            return Err(Error::malformed(
                "the projection names a column this dataset does not have",
            ));
        }

        let start = request.row_start().min(self.rows);
        let wanted = request.row_count().min(self.rows - start);

        // One batch per chunk, reusing the same allocations. A host that asked for ten million rows
        // gets them in pieces it can start working on, which is the whole reason the sink takes a
        // batch at a time rather than returning a list.
        let mut batch = Batch::new(0);
        let mut done = 0;
        while done < wanted {
            let count = self.batch_rows.min(wanted - done);
            batch.reset(count);
            for &column in &columns {
                let offset = self.offset(column, start + done);
                let bytes = source.range(offset, count * WIDTH)?;
                batch.array(count, 0);
                // An empty validity buffer is how a batch says every value is present. The entry
                // still has to be there, because the schema decides how many buffers there are and
                // leaving one out would shift every buffer after it.
                batch.buffer(&[]);
                batch.buffer(bytes);
            }
            sink.emit(&batch)?;
            done += count;
        }
        Ok(())
    }
}

export_decoder!(FixedWidth);

/// A decoder is a `cdylib` and has no `main`. An example is a binary, so it gets one, and the most
/// useful thing to put in it is the decode loop being driven with no host and no runtime at all.
fn main() {
    use iris_abi::{ABI_MAJOR, ABI_MINOR, Hello, ScanRequest};
    use iris_decoder::{Collect, Instance, Resident, record};

    let rows = 1000u64;
    let columns = 3u64;
    let mut source = Vec::new();
    source.extend_from_slice(&rows.to_le_bytes());
    source.extend_from_slice(&columns.to_le_bytes());
    for column in 0..columns {
        for row in 0..rows {
            source.extend_from_slice(&(column * 1000 + row).to_le_bytes());
        }
    }

    let hello = Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        window_bytes: 0,
        max_batch_rows: 256,
        offered: CapabilitySet::new()
            .with(Capability::RANDOM_ACCESS)
            .with(Capability::PROJECTION),
        source_bytes: source.len() as u64,
    };

    let mut instance = Instance::<FixedWidth>::new();
    let greeting = record(|w| hello.encode(w)).expect("a Hello always fits");
    instance.input(greeting.len()).copy_from_slice(&greeting);
    instance.start(&mut Resident::new(&source));

    let request = ScanRequest {
        row_count: rows,
        ..ScanRequest::everything()
    };
    let encoded = record(|w| request.encode(w)).expect("a ScanRequest always fits");
    instance.input(encoded.len()).copy_from_slice(&encoded);

    let mut sink = Collect::new();
    instance.scan(&mut Resident::new(&source), &mut sink);
    println!("{} rows in {} batches", sink.rows(), sink.batches().len());
}
