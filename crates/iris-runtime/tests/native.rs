//! Substitution is keyed on the digest, and on nothing a container can assert about itself.
//!
//! A host that recognises a decoder may run its own implementation of it and skip the sandbox. The
//! obvious way to decide what "recognises" means is the decoder's name, and the obvious way is
//! unsound: the name is written by whoever wrote the container, so a dataset that calls its decoder
//! `fixedwidth` would get this host's native code, running in this process with nothing around it,
//! against bytes nobody has ever compared it to. These tests are about that not happening.
//!
//! The native implementation below returns deliberately wrong values, offset by [`MARK`]. That is
//! the only way for the rows themselves to say which side produced them, and it is fine here because
//! this is a test decoder and being wrong is its whole job. A real native kernel has to agree with
//! the module it stands in for, and the box that makes it prove that is the differential run, which
//! is the next one in the milestone rather than this one.

mod support;

use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;

use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Hello, Node, ScanRequest};
use iris_format::{Builder, Digest, SchemaEncoding, SectionKind};
use iris_native::{Error, Native, Registry, Result, Scanning};
use iris_runtime::{Runtime, schema_to_ipc};
use iris_source::{Fetch, MemorySource, RangeSource, SourceError};
use iris_vm::{Handshake, RawBatch};

use support::{
    HEADER, WIDTH, builder, cell, column_values, decoder_module, flat_source, passthrough_module,
    schema,
};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// Two, so a projection has something to leave out.
const COLUMNS: u64 = 2;

/// What the native implementation adds to every value it produces.
///
/// Far outside the range [`cell`] produces, so a value that came back from here cannot be mistaken
/// for one the module in the container produced, and the assertion failure says which path ran
/// rather than being an off by one.
const MARK: i64 = 700_000_000_000;

/// A native implementation of the fixed width decoder that is wrong on purpose.
///
/// It reads the same layout the module reads, honours the same request, and then adds [`MARK`] to
/// every value on the way out. See the note at the top of this file for why it is like that.
#[derive(Debug)]
struct Rewrite;

impl Native for Rewrite {
    fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
        // The same terms the module asks for, because this stands in for that module and a
        // substitution that negotiated differently would be a different decoder.
        Ok(Handshake {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            required: CapabilitySet::new().with(Capability::RANDOM_ACCESS),
            optional: CapabilitySet::new().with(Capability::PROJECTION),
            decoder_id: "fixedwidth".to_owned(),
        })
    }

    fn scan<'a>(
        &'a self,
        request: &'a ScanRequest<'a>,
        source: &'a mut (dyn RangeSource + Send),
    ) -> Scanning<'a> {
        Box::pin(async move {
            let header = read(source, 0, 16).await?;
            let rows = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
            let columns = u64::from_le_bytes(header[8..].try_into().expect("eight bytes"));

            let wanted: Vec<u64> = if request.projection.is_empty() {
                (0..columns).collect()
            } else {
                request.projection.iter().map(u64::from).collect()
            };
            if wanted.iter().any(|&column| column >= columns) {
                return Err(Error::malformed(
                    "the projection names a column this dataset does not have",
                ));
            }

            let start = request.row_start.min(rows);
            let count = request.row_count.min(rows - start);
            let mut batch = RawBatch {
                rows: count,
                nodes: Vec::new(),
                buffers: Vec::new(),
            };
            for column in wanted {
                let at = HEADER + (column * rows + start) * WIDTH;
                let len = usize::try_from(count * WIDTH).expect("a test fixture fits in memory");
                let values = read(source, at, len).await?;
                let marked: Vec<u8> = values
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|slot| i64::from_le_bytes(*slot) + MARK)
                    .flat_map(i64::to_le_bytes)
                    .collect();

                batch.nodes.push(Node {
                    length: count,
                    null_count: 0,
                });
                // An empty validity buffer is how a batch says every value is present. The entry
                // still has to be there, because the schema decides how many buffers there are.
                batch.buffers.push(Vec::new());
                batch.buffers.push(marked);
            }
            Ok(vec![batch])
        })
    }
}

/// Asks a source for a range and waits for it without holding a thread.
///
/// A source that says it will wake the task is taken at its word, and one that says it will not is
/// asked again on the next poll, which is the arrangement `RangeSource::wake_when_ready` describes.
/// Nothing in this file actually goes pending, since both fixtures are resident, and it is written
/// this way because a native implementation that spun here would give back the property M6 was
/// about.
async fn read(
    source: &mut (dyn RangeSource + Send),
    at: u64,
    len: usize,
) -> std::result::Result<Vec<u8>, SourceError> {
    poll_fn(|cx| {
        match source.range(at, len) {
            Err(err) => return Poll::Ready(Err(err)),
            Ok(Fetch::Ready(bytes)) => return Poll::Ready(Ok(bytes.to_vec())),
            // Pending, and anything a later version of the trait adds, means come back later.
            Ok(_) => {}
        }
        if !source.wake_when_ready(cx.waker()) {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

/// A registry that runs [`Rewrite`] in place of the module the fixtures carry.
fn registry() -> Registry {
    Registry::new().with_module(decoder_module(), Arc::new(Rewrite))
}

/// The ordinary fixture: the fixed width module, the data it reads, and the name it goes by.
fn container() -> Vec<u8> {
    builder(ROWS, COLUMNS)
        .build()
        .expect("the container is writable")
}

/// A container that calls its decoder `fixedwidth` and carries a different decoder entirely.
///
/// This is the attack the digest exists to stop, written out. The name in the decoder record says
/// `fixedwidth`, the module in the section is the passthrough decoder, and the data section is laid
/// out the way passthrough reads it. A host that keyed substitution on the name would run the native
/// fixed width implementation over these bytes, read a header that is not there, and produce
/// whatever came of that. A host that keys on the digest sees a decoder it has never heard of.
fn impostor() -> Vec<u8> {
    let mut builder = Builder::new("readings", ROWS);
    builder.schema(
        SchemaEncoding::ArrowIpc,
        schema_to_ipc(&schema(1)).expect("one integer column always encodes"),
    );
    builder.section(SectionKind::Data, flat_source(ROWS));
    builder.embed_decoder(
        "fixedwidth",
        (ABI_MAJOR, ABI_MINOR),
        CapabilitySet::new().with(Capability::RANDOM_ACCESS),
        passthrough_module().to_vec(),
    );
    builder.build().expect("the container is writable")
}

/// What a column reads as when the module in the container decoded it.
fn plain(column: u64) -> Vec<i64> {
    (0..ROWS).map(|row| cell(column, row)).collect()
}

/// What a column reads as when [`Rewrite`] decoded it.
fn marked(column: u64) -> Vec<i64> {
    (0..ROWS).map(|row| cell(column, row) + MARK).collect()
}

#[test]
fn a_decoder_registered_under_its_own_digest_runs_native() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry());
    let dataset = runtime.open(&bytes).expect("the container opens");

    assert!(
        dataset.decoder_is_native(),
        "the registry holds this exact module, so the host runs its own implementation"
    );

    let batches = dataset.scan().expect("the scan runs");
    assert_eq!(column_values(&batches, 0), marked(0));
    assert_eq!(column_values(&batches, 1), marked(1));

    assert_eq!(
        runtime.decoders_compiled(),
        0,
        "substitution skips the sandbox, so nothing was compiled at all"
    );
}

#[test]
fn a_decoder_claiming_a_known_name_with_unknown_bytes_gets_the_sandbox() {
    let bytes = impostor();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry());
    let dataset = runtime.open(&bytes).expect("the container opens");

    assert!(
        !dataset.decoder_is_native(),
        "the name matches what the registry was built for and the bytes do not, and the bytes decide"
    );
    assert_ne!(
        dataset.decoder_digest(),
        Digest::of(decoder_module()),
        "this container carries a different module, which is the whole premise of the test"
    );

    // And the rows are the ones the module in the container produces, which is the other half of
    // the claim. It is not enough for the host to decline to substitute: the scan still has to work,
    // through the sandbox, exactly as it would on a host that had never heard of native code.
    let batches = dataset.scan().expect("the scan runs");
    assert_eq!(column_values(&batches, 0), plain(0));
    assert_eq!(
        runtime.decoders_compiled(),
        1,
        "the sandbox path ran, so the module was compiled"
    );
}

#[test]
fn an_empty_registry_is_every_container_in_the_sandbox() {
    let bytes = container();
    let runtime = Runtime::new().expect("a runtime starts");
    let dataset = runtime.open(&bytes).expect("the container opens");

    assert!(!dataset.decoder_is_native());
    assert_eq!(
        column_values(&dataset.scan().expect("the scan runs"), 0),
        plain(0)
    );
}

#[test]
fn registering_by_module_computes_the_digest_the_container_carries() {
    let bytes = container();
    let dataset = Runtime::new()
        .expect("a runtime starts")
        .open(&bytes)
        .expect("the container opens");

    // The two ways of naming a decoder agree. A host holding the module it wrote a rewrite of does
    // not have to copy a hex string out of anywhere, which is worth saying with a test because a
    // mistyped digest fails by silently never substituting.
    assert_eq!(dataset.decoder_digest(), Digest::of(decoder_module()));
    assert!(registry().get(&dataset.decoder_digest()).is_some());
}

#[test]
fn substitution_is_the_same_decision_on_the_windowed_path() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry());
    let mut dataset = runtime
        .open_windowed(Box::new(MemorySource::new(bytes)))
        .expect("the container opens");

    assert!(dataset.decoder_is_native());
    let batches = dataset.scan().expect("the scan runs");
    assert_eq!(column_values(&batches, 0), marked(0));
}

#[test]
fn a_projection_still_goes_through_the_host_when_the_decoder_is_native() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry());
    let dataset = runtime.open(&bytes).expect("the container opens");

    let batches = dataset.scan_columns(&[1]).expect("the scan runs");
    assert_eq!(batches[0].num_columns(), 1);
    assert_eq!(batches[0].schema().field(0).name(), "c1");
    assert_eq!(column_values(&batches, 0), marked(1));

    // The bounds check on a projection belongs to the host and stays there. Native code is not
    // trusted with it any more than a guest is, and the error names the index rather than being
    // whatever the implementation decided to do about a column that is not there.
    let one_past_the_end = u32::try_from(COLUMNS).expect("two columns fit in a u32");
    let err = dataset
        .scan_columns(&[one_past_the_end])
        .expect_err("column two of a two column dataset is not a column");
    assert!(
        err.to_string().contains("names column 2"),
        "the host's own message, not the decoder's: {err}"
    );
}

#[test]
fn what_a_native_implementation_emits_is_checked_like_anything_else() {
    /// Claims more rows than it hands over buffers for.
    ///
    /// A guest that did this is refused by `iris-guard` before an array is built. Native code has no
    /// sandbox around it and is easier to trust for that reason, and it is decoding a dataset the
    /// host did not write, so it gets the same treatment.
    #[derive(Debug)]
    struct Liar;

    impl Native for Liar {
        fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
            Ok(Handshake {
                abi_major: ABI_MAJOR,
                abi_minor: ABI_MINOR,
                required: CapabilitySet::new().with(Capability::RANDOM_ACCESS),
                optional: CapabilitySet::new(),
                decoder_id: "liar".to_owned(),
            })
        }

        fn scan<'a>(
            &'a self,
            _request: &'a ScanRequest<'a>,
            _source: &'a mut (dyn RangeSource + Send),
        ) -> Scanning<'a> {
            Box::pin(async {
                Ok(vec![RawBatch {
                    rows: ROWS,
                    nodes: (0..COLUMNS)
                        .map(|_| Node {
                            length: ROWS,
                            null_count: 0,
                        })
                        .collect(),
                    // One value per column, and a promise of five hundred and twelve.
                    buffers: (0..COLUMNS)
                        .flat_map(|_| [Vec::new(), 1i64.to_le_bytes().to_vec()])
                        .collect(),
                }])
            })
        }
    }

    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(Registry::new().with_module(decoder_module(), Arc::new(Liar)));
    let dataset = runtime.open(&bytes).expect("the container opens");

    let err = dataset
        .scan()
        .expect_err("a buffer eight bytes long does not hold five hundred and twelve values");
    assert!(
        matches!(err, iris_runtime::Error::Guard(_)),
        "the guard is what refuses this, on both paths: {err}"
    );
}
