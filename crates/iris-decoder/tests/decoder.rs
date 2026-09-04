//! A decoder written against the SDK, and the conversation a host has with it.
//!
//! The decoder below is deliberately boring. Two columns of fixed width integers with a row count
//! on the front, no compression, no nulls. What is being tested is the contract, not the decoding,
//! and the useful measure of the SDK is how much of this file is decode logic and how much is
//! ceremony.

use iris_abi::{
    ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Hello, Message, Projection, Reader,
    RefusalReason, ScanRequest,
};
use iris_decoder::{
    Batch, Collect, Decoder, Error, Instance, Request, Resident, Result, Session, Sink, Source,
    record,
};

/// Where the values start, which is just past the row count on the front.
const HEADER: u64 = 8;

/// Two columns: eight byte integers and then four byte integers, both with the same row count.
struct Pair {
    rows: u64,
}

impl Pair {
    /// Where the four byte column starts, which depends on how long the eight byte one is.
    const fn second_column(&self) -> u64 {
        HEADER + self.rows * 8
    }
}

impl Decoder for Pair {
    const NAME: &'static str = "pair";
    const REQUIRES: CapabilitySet = CapabilitySet::new().with(Capability::RANDOM_ACCESS);
    const OPTIONAL: CapabilitySet = CapabilitySet::new().with(Capability::PROJECTION);

    fn open(_session: &Session, source: &mut dyn Source) -> Result<Self> {
        let head = source.range(0, HEADER)?;
        let rows = u64::from_le_bytes(
            head.try_into()
                .map_err(|_| Error::malformed("the row count on the front is the wrong width"))?,
        );
        Ok(Self { rows })
    }

    fn scan(
        &mut self,
        request: &Request<'_>,
        source: &mut dyn Source,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        let start = request.row_start().min(self.rows);
        let count = request.row_count().min(self.rows - start);
        if count == 0 {
            return Ok(());
        }

        let (mut wide, mut narrow) = (true, true);
        if let Some(columns) = request.columns() {
            (wide, narrow) = (false, false);
            for column in columns {
                match column {
                    0 => wide = true,
                    1 => narrow = true,
                    _ => return Err(Error::malformed("this decoder has two columns")),
                }
            }
        }

        let mut batch = Batch::new(count);
        if wide {
            let bytes = source.range(HEADER + start * 8, count * 8)?;
            batch.array(count, 0).buffer(&[]).buffer(bytes);
        }
        if narrow {
            let bytes = source.range(self.second_column() + start * 4, count * 4)?;
            batch.array(count, 0).buffer(&[]).buffer(bytes);
        }
        sink.emit(&batch)
    }
}

/// A source with `rows` rows in it, where row `i` holds `i` in both columns.
fn dataset(rows: u64) -> Vec<u8> {
    let mut out = rows.to_le_bytes().to_vec();
    for i in 0..rows {
        out.extend_from_slice(&i.to_le_bytes());
    }
    for i in 0..rows {
        out.extend_from_slice(&u32::try_from(i).unwrap().to_le_bytes());
    }
    out
}

fn hello(source: &[u8]) -> Hello {
    Hello {
        abi_major: ABI_MAJOR,
        abi_minor: ABI_MINOR,
        window_bytes: 0,
        max_batch_rows: 8192,
        offered: CapabilitySet::new()
            .with(Capability::RANDOM_ACCESS)
            .with(Capability::PROJECTION),
        source_bytes: u64::try_from(source.len()).unwrap(),
    }
}

/// Puts a record in the input buffer and returns whatever the decoder said back.
fn send(instance: &mut Instance<Pair>, source: &[u8], message: &[u8], start: bool) -> Vec<u8> {
    instance.input(message.len()).copy_from_slice(message);
    let mut resident = Resident::new(source);
    if start {
        instance.start(&mut resident).to_vec()
    } else {
        let mut sink = Collect::new();
        instance.scan(&mut resident, &mut sink).to_vec()
    }
}

/// Opens a decoder over a dataset, asserting that it opened.
fn opened(source: &[u8]) -> Instance<Pair> {
    let mut instance = Instance::new();
    let greeting = record(|w| hello(source).encode(w)).unwrap();
    let answer = send(&mut instance, source, &greeting, true);
    assert!(matches!(
        Reader::new(&answer).message().unwrap(),
        Message::HelloAck(_)
    ));
    instance
}

/// Runs one scan and hands back everything the decoder emitted.
fn scan(instance: &mut Instance<Pair>, source: &[u8], request: &ScanRequest<'_>) -> Collect {
    let encoded = record(|w| request.encode(w)).unwrap();
    instance.input(encoded.len()).copy_from_slice(&encoded);
    let mut resident = Resident::new(source);
    let mut sink = Collect::new();
    assert!(
        instance.scan(&mut resident, &mut sink).is_empty(),
        "the scan should have finished without refusing"
    );
    sink
}

#[test]
fn a_decoder_that_opens_says_who_it_is() {
    let source = dataset(100);
    let mut instance = Instance::<Pair>::new();
    let greeting = record(|w| hello(&source).encode(w)).unwrap();
    let answer = send(&mut instance, &source, &greeting, true);

    let Message::HelloAck(ack) = Reader::new(&answer).message().unwrap() else {
        panic!("a decoder that opened answers with a HelloAck");
    };
    assert_eq!(ack.decoder_id, "pair");
    assert_eq!(ack.abi_major, ABI_MAJOR);
    assert!(ack.required.contains(Capability::RANDOM_ACCESS));
    assert!(ack.optional.contains(Capability::PROJECTION));
    assert!(instance.is_open());
}

#[test]
fn a_host_that_does_not_offer_what_the_decoder_needs_is_told_which_bit_was_missing() {
    let source = dataset(10);
    let bare = Hello {
        offered: CapabilitySet::new(),
        ..hello(&source)
    };
    let mut instance = Instance::<Pair>::new();
    let greeting = record(|w| bare.encode(w)).unwrap();
    let answer = send(&mut instance, &source, &greeting, true);

    let Message::Refusal(refusal) = Reader::new(&answer).message().unwrap() else {
        panic!("a decoder that cannot run refuses");
    };
    assert_eq!(refusal.reason, RefusalReason::MISSING_CAPABILITY);
    assert_eq!(refusal.capability, Capability::RANDOM_ACCESS);
    // The whole reason a refusal is a record and not a dropped connection.
    assert!(!refusal.detail.is_empty());
    assert!(!instance.is_open());
}

#[test]
fn a_scan_that_arrives_before_the_decoder_is_open_is_refused() {
    let source = dataset(10);
    let mut instance = Instance::<Pair>::new();
    let request = record(|w| ScanRequest::everything().encode(w)).unwrap();
    let answer = send(&mut instance, &source, &request, false);

    let Message::Refusal(refusal) = Reader::new(&answer).message().unwrap() else {
        panic!("a scan before a start is refused");
    };
    assert_eq!(refusal.reason, RefusalReason::UNSUPPORTED_RECORD);
}

#[test]
fn a_decoder_handed_the_wrong_record_says_so_rather_than_guessing() {
    let source = dataset(10);
    let mut instance = Instance::<Pair>::new();
    // A ScanRequest where a Hello belongs.
    let request = record(|w| ScanRequest::everything().encode(w)).unwrap();
    let answer = send(&mut instance, &source, &request, true);

    let Message::Refusal(refusal) = Reader::new(&answer).message().unwrap() else {
        panic!("an unexpected record is refused");
    };
    assert_eq!(refusal.reason, RefusalReason::UNSUPPORTED_RECORD);
}

#[test]
fn a_scan_produces_the_rows_it_was_asked_for() {
    let source = dataset(1_000);
    let mut instance = opened(&source);
    let sink = scan(
        &mut instance,
        &source,
        &ScanRequest {
            row_start: 400,
            row_count: 250,
            ..ScanRequest::everything()
        },
    );

    assert_eq!(sink.rows(), 250);
    let batch = &sink.batches()[0];
    assert_eq!(batch.nodes.len(), 2);
    assert!(
        batch
            .nodes
            .iter()
            .all(|n| n.length == 250 && n.null_count == 0)
    );

    // Two arrays, each with an absent validity buffer and a values buffer.
    assert_eq!(batch.buffers.len(), 4);
    assert!(batch.buffers[0].is_empty());
    assert_eq!(batch.buffers[1].len(), 250 * 8);
    assert!(batch.buffers[2].is_empty());
    assert_eq!(batch.buffers[3].len(), 250 * 4);

    // Row 400 of the wide column holds 400, and the last row of the narrow one holds 649.
    assert_eq!(
        u64::from_le_bytes(batch.buffers[1][..8].try_into().unwrap()),
        400
    );
    let last = &batch.buffers[3][249 * 4..];
    assert_eq!(u32::from_le_bytes(last.try_into().unwrap()), 649);
}

#[test]
fn an_empty_projection_means_every_column_rather_than_no_columns() {
    let source = dataset(16);
    let mut instance = opened(&source);

    let all = scan(&mut instance, &source, &ScanRequest::everything());
    assert_eq!(all.batches()[0].nodes.len(), 2);

    let narrow = scan(
        &mut instance,
        &source,
        &ScanRequest {
            projection: Projection::from_bytes(&[1, 0, 0, 0]).unwrap(),
            ..ScanRequest::everything()
        },
    );
    assert_eq!(narrow.batches()[0].nodes.len(), 1);
    assert_eq!(narrow.batches()[0].buffers[1].len(), 16 * 4);
}

#[test]
fn a_scan_that_starts_past_the_end_produces_nothing_and_is_not_an_error() {
    let source = dataset(16);
    let mut instance = opened(&source);
    let sink = scan(
        &mut instance,
        &source,
        &ScanRequest {
            row_start: 1_000,
            ..ScanRequest::everything()
        },
    );
    assert_eq!(sink.rows(), 0);
    assert!(sink.batches().is_empty());
}

#[test]
fn a_reserved_flag_on_a_scan_request_is_refused_rather_than_ignored() {
    let source = dataset(16);
    let mut instance = opened(&source);
    let request = record(|w| {
        ScanRequest {
            flags: 1,
            ..ScanRequest::everything()
        }
        .encode(w)
    })
    .unwrap();
    let answer = send(&mut instance, &source, &request, false);

    let Message::Refusal(refusal) = Reader::new(&answer).message().unwrap() else {
        panic!("a flag nobody has defined yet is not something to guess at");
    };
    assert_eq!(refusal.reason, RefusalReason::UNSUPPORTED_RECORD);
}

#[test]
fn a_range_that_runs_off_the_end_of_the_source_is_refused() {
    let source = dataset(4);
    // The header says there are four hundred rows and the file holds four, which is what a
    // truncated dataset looks like from inside a decoder.
    let mut lying = source.clone();
    lying[..8].copy_from_slice(&400u64.to_le_bytes());

    let mut instance = opened(&lying);
    let encoded = record(|w| ScanRequest::everything().encode(w)).unwrap();
    let answer = send(&mut instance, &lying, &encoded, false);

    let Message::Refusal(refusal) = Reader::new(&answer).message().unwrap() else {
        panic!("a range past the end is a refusal");
    };
    assert_eq!(refusal.reason, RefusalReason::MALFORMED);
}

#[test]
fn the_batch_record_describes_the_buffers_the_decoder_actually_built() {
    let mut batch = Batch::new(3);
    batch.array(3, 1).buffer(&[0b0000_0101]).buffer(&[
        1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0,
    ]);

    let mut encoded = Vec::new();
    batch.record(&mut encoded).unwrap();

    let Message::Batch(decoded) = Reader::new(&encoded).message().unwrap() else {
        panic!("that was a batch");
    };
    assert_eq!(decoded.rows, 3);
    assert_eq!(decoded.flags, 0);

    let nodes: Vec<_> = decoded.nodes.iter().collect();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].length, 3);
    assert_eq!(nodes[0].null_count, 1);

    // Every reference points at where the buffer really is and says how long it really is. The
    // addresses are only meaningful in this process, which is exactly the situation the host is in
    // when it reads them out of guest memory.
    let refs: Vec<_> = decoded.buffers.iter().collect();
    assert_eq!(refs.len(), 2);
    for (reference, buffer) in refs.iter().zip(batch.buffers()) {
        assert_eq!(reference.offset, buffer.as_ptr() as usize as u64);
        assert_eq!(reference.len, u64::try_from(buffer.len()).unwrap());
    }
}

#[test]
fn a_batch_that_was_reset_describes_nothing_from_the_batch_before_it() {
    let mut batch = Batch::new(2);
    batch.array(2, 0).buffer(&[]).buffer(&[7; 16]);
    assert!(!batch.is_empty());

    batch.reset(5);
    assert!(batch.is_empty());
    assert_eq!(batch.rows(), 5);
    assert_eq!(batch.nodes().len(), 0);
    assert_eq!(batch.buffers().len(), 0);
}
