//! The smallest decoder that is still a decoder, so that the exported surface gets compiled.
//!
//! The interesting part of this file is the last line. Everything above it is the least a type can
//! do and still implement [`Decoder`], and the point of the example is that building it for
//! `wasm32-unknown-unknown` type checks what [`iris_decoder::export_decoder`] generates. A macro
//! that is only ever expanded on a target it does nothing on is a macro nobody is compiling.

use iris_abi::{Capability, CapabilitySet};
use iris_decoder::{Batch, Decoder, Request, Result, Session, Sink, Source, export_decoder};

/// One column of eight byte integers, read straight out of the source with no header at all.
struct Passthrough;

impl Decoder for Passthrough {
    const NAME: &'static str = "passthrough";
    const REQUIRES: CapabilitySet = CapabilitySet::new().with(Capability::RANDOM_ACCESS);

    fn open(_session: &Session, _source: &mut dyn Source) -> Result<Self> {
        Ok(Self)
    }

    fn scan(
        &mut self,
        request: &Request<'_>,
        source: &mut dyn Source,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        let rows = request.row_count().min(1024);
        if rows == 0 {
            return Ok(());
        }
        let mut batch = Batch::new(rows);
        let bytes = source.range(request.row_start() * 8, rows * 8)?;
        batch.array(rows, 0).buffer(&[]).buffer(bytes);
        sink.emit(&batch)
    }
}

export_decoder!(Passthrough);

/// A real decoder is a `cdylib` and has no `main`. An example is a binary, so it gets one, and the
/// most useful thing to put in it is the decoder being driven without a host at all.
fn main() {
    use iris_abi::{ABI_MAJOR, ABI_MINOR, Hello, ScanRequest};
    use iris_decoder::{Collect, Instance, Resident, record};

    let source: Vec<u8> = (0u64..64).flat_map(u64::to_le_bytes).collect();

    let hello = Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        window_bytes: 0,
        max_batch_rows: 1024,
        offered: CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        source_bytes: u64::try_from(source.len()).expect("a source this small fits in a u64"),
    };

    let mut instance = Instance::<Passthrough>::new();
    let greeting = record(|w| hello.encode(w)).expect("a Hello always fits");
    instance.input(greeting.len()).copy_from_slice(&greeting);
    instance.start(&mut Resident::new(&source));

    let request = ScanRequest {
        row_count: 64,
        ..ScanRequest::everything()
    };
    let encoded = record(|w| request.encode(w)).expect("a ScanRequest always fits");
    instance.input(encoded.len()).copy_from_slice(&encoded);

    let mut sink = Collect::new();
    instance.scan(&mut Resident::new(&source), &mut sink);
    println!("{} rows in {} batches", sink.rows(), sink.batches().len());
}
