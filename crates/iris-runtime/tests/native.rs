//! Substitution is keyed on the digest, and on nothing a container can assert about itself.
//!
//! A host that recognises a decoder may run its own implementation of it and skip the sandbox. The
//! obvious way to decide what "recognises" means is the decoder's name, and the obvious way is
//! unsound: the name is written by whoever wrote the container, so a dataset that calls its decoder
//! `fixedwidth` would get this host's native code, running in this process with nothing around it,
//! against bytes nobody has ever compared it to. These tests are about that not happening.
//!
//! The implementation they register is a real one. It reads the layout the module reads and produces
//! the same batches, and it reaches a registry the only way anything reaches one, which is by coming
//! out of a differential run against the module it stands in for. That means these tests cannot tell
//! the two paths apart by looking at the values, because agreeing on the values is the entry
//! requirement. They tell them apart by asking the dataset which path it took and by asking the
//! runtime how many decoders it compiled, which is the honest question anyway: substitution is
//! supposed to be invisible in the answers and visible in the work.

mod support;

use std::sync::Arc;

use iris_abi::{ABI_MAJOR, ABI_MINOR, Capability, CapabilitySet, Hello, Node, ScanRequest};
use iris_format::{Builder, Digest, SchemaEncoding, SectionKind};
use iris_native::{Native, Registry, Result, Scanning};
use iris_runtime::{RESIDENT_TERMS, Runtime, WINDOWED_TERMS, schema_to_ipc};
use iris_source::{MemorySource, RangeSource};
use iris_vm::{Handshake, RawBatch};

use support::{
    CORPUS_ROWS, FixedWidth, attempt, builder, cell, column_values, decoder_module,
    fixedwidth_batches, fixedwidth_handshake, flat_source, native_corpus, passthrough_module,
    prove, schema,
};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// Two, so a projection has something to leave out.
const COLUMNS: u64 = 2;

/// A registry that runs the native fixed width implementation in place of the module.
fn registry() -> Registry {
    Registry::new().with(prove(Arc::new(FixedWidth)))
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

/// What a column of the fixture reads as, whichever side decoded it.
fn plain(column: u64) -> Vec<i64> {
    (0..ROWS).map(|row| cell(column, row)).collect()
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
    assert_eq!(column_values(&batches, 0), plain(0));
    assert_eq!(column_values(&batches, 1), plain(1));

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
fn a_kernel_carries_the_digest_of_the_module_it_was_proved_against() {
    let bytes = container();
    let dataset = Runtime::new()
        .expect("a runtime starts")
        .open(&bytes)
        .expect("the container opens");

    // Nobody types a digest anywhere. The differential run compiled the module it was handed, so the
    // key a kernel goes in under is derived from the same bytes the container carries, which is worth
    // saying with a test because a mistyped digest fails by silently never substituting.
    assert_eq!(dataset.decoder_digest(), Digest::of(decoder_module()));
    assert!(
        registry()
            .get(&dataset.decoder_digest(), RESIDENT_TERMS)
            .is_some()
    );
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
    assert_eq!(column_values(&batches, 0), plain(0));
    assert_eq!(
        runtime.decoders_compiled(),
        0,
        "the windowed path substituted too, so there was still nothing to compile"
    );
}

#[test]
fn a_kernel_proved_for_one_path_is_not_run_on_the_other() {
    // The same implementation, proved under the terms the resident path offers and under nothing
    // else. It would in fact be right on the windowed path as well, and that is the point: nobody
    // has compared it there, so the host declines to assume it and falls back to the sandbox.
    let kernel = attempt(Arc::new(FixedWidth), &native_corpus(), &[RESIDENT_TERMS])
        .expect("the implementation agrees with the module under the resident terms");
    let registry = Registry::new().with(kernel);

    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry);

    assert!(
        runtime
            .open(&bytes)
            .expect("the container opens")
            .decoder_is_native(),
        "the terms this was proved under are the ones the resident path offers"
    );

    let mut windowed = runtime
        .open_windowed(Box::new(MemorySource::new(bytes)))
        .expect("the container opens");
    assert!(
        !windowed.decoder_is_native(),
        "the windowed path offers more than anybody ran this implementation under"
    );
    assert_eq!(
        column_values(&windowed.scan().expect("the scan runs"), 0),
        plain(0),
        "and the fallback is a working scan rather than a failure"
    );
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
    assert_eq!(column_values(&batches, 0), plain(1));

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
    /// Right about every dataset the corpus holds and wrong about a larger one.
    ///
    /// This is the residual a differential run leaves behind, written out on purpose. The run proves
    /// agreement on the cases it was given, and the cases it was given are the cases somebody thought
    /// of, so a kernel that is wrong on the dataset nobody thought of still gets registered. There is
    /// no arrangement of the type system that closes that, and pretending otherwise would be worse
    /// than saying it.
    ///
    /// What does close it is that nothing about being native gets a batch believed. This one claims
    /// more rows than it hands over buffers for, `iris-guard` refuses it before an array is built,
    /// and it is the same refusal a guest that did this would get.
    #[derive(Debug)]
    struct Deceitful;

    impl Native for Deceitful {
        fn identity(&self) -> &'static str {
            "deceitful 1.0.0"
        }

        fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
            Ok(fixedwidth_handshake())
        }

        fn scan<'a>(
            &'a self,
            hello: &'a Hello,
            request: &'a ScanRequest<'a>,
            source: &'a mut (dyn RangeSource + Send),
        ) -> Scanning<'a> {
            Box::pin(async move {
                let header = support::read(source, 0, 16).await?;
                let rows = u64::from_le_bytes(header[..8].try_into().expect("eight bytes"));
                if rows <= CORPUS_ROWS {
                    return fixedwidth_batches(hello, request, source).await;
                }
                Ok(vec![RawBatch {
                    rows,
                    nodes: (0..COLUMNS)
                        .map(|_| Node {
                            length: rows,
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

    // It passes, because every case in the corpus is smaller than the dataset it lies about.
    let registry = Registry::new().with(prove(Arc::new(Deceitful)));

    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(registry);
    let dataset = runtime.open(&bytes).expect("the container opens");
    assert!(dataset.decoder_is_native());

    let err = dataset
        .scan()
        .expect_err("a buffer eight bytes long does not hold five hundred and twelve values");
    assert!(
        matches!(err, iris_runtime::Error::Guard(_)),
        "the guard is what refuses this, on both paths: {err}"
    );
}

#[test]
fn the_terms_a_kernel_was_proved_under_are_the_terms_it_records() {
    let kernel = prove(Arc::new(FixedWidth));
    assert_eq!(kernel.digest(), Digest::of(decoder_module()));
    assert_eq!(kernel.offered(), [RESIDENT_TERMS, WINDOWED_TERMS]);
}
