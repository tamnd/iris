//! What a differential run catches, checked against a real module and real native code.
//!
//! Registering a native implementation asserts that it produces, for every dataset the module reads,
//! exactly what running the module would have produced. The only way to make that assertion is to run
//! one of these, because the only thing that builds a kernel is a passing run and the only thing a
//! registry accepts is a kernel. So the question these tests ask is whether the run is worth
//! standing behind, which means writing implementations that are wrong in the ways a rewrite is
//! actually wrong and checking that each one comes back named.
//!
//! They live here rather than in `iris-native` because a differential run needs a module, and this
//! is the crate whose tests build one. `iris-native` sits below the decoder SDK and cannot reach a
//! compiled decoder without inverting the dependency graph to get at it.

mod support;

use std::sync::Arc;

use iris_abi::{CapabilitySet, Hello, ScanRequest};
use iris_format::Digest;
use iris_native::{Case, Corpus, Mismatch, Native, Result, Scanning};
use iris_runtime::{RESIDENT_TERMS, WINDOWED_TERMS};
use iris_source::RangeSource;
use iris_vm::Handshake;

use support::{
    FixedWidth, attempt, decoder_module, fixedwidth_batches, fixedwidth_handshake, native_corpus,
    prove, source,
};

/// Both sets of terms this host offers, which is what a kernel meant for both paths is proved under.
fn both() -> Vec<CapabilitySet> {
    vec![RESIDENT_TERMS, WINDOWED_TERMS]
}

/// Runs the fixed width differential against an implementation expected to fail, and returns why.
fn failure(native: Arc<dyn Native>) -> Mismatch {
    attempt(native, &native_corpus(), &both())
        .expect_err("this implementation disagrees with the module")
}

#[test]
fn an_implementation_that_agrees_with_the_module_becomes_a_kernel() {
    let kernel = prove(Arc::new(FixedWidth));

    assert_eq!(
        kernel.digest(),
        Digest::of(decoder_module()),
        "the digest comes out of the run rather than out of a caller, so it is the module's own"
    );
    assert_eq!(kernel.offered(), both());
}

#[test]
fn one_wrong_bit_in_one_value_is_reported_as_the_byte_it_is() {
    /// Decodes the layout correctly and then flips the lowest bit of the first value it returns.
    ///
    /// This is the failure the whole harness exists for. Two implementations of the same format, one
    /// of them wrong somewhere small, and the wrong one is the one that runs. Reading both outputs
    /// side by side would not find it and neither would a spot check of the totals.
    #[derive(Debug)]
    struct Skewed;

    impl Native for Skewed {
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
                let mut batches = fixedwidth_batches(hello, request, source).await?;
                if let Some(values) = batches
                    .iter_mut()
                    .flat_map(|batch| batch.buffers.iter_mut())
                    .find(|buffer| !buffer.is_empty())
                {
                    values[0] ^= 1;
                }
                Ok(batches)
            })
        }
    }

    let err = failure(Arc::new(Skewed));
    assert!(
        matches!(err, Mismatch::Bytes { at: 0, .. }),
        "the run says which case, which request, which buffer and which byte: {err}"
    );
}

#[test]
fn an_implementation_that_ignores_the_batch_size_is_caught() {
    /// Right about every row and wrong about how they are handed over.
    ///
    /// It decodes the layout properly and then puts the whole answer in one batch, whatever the host
    /// said about `max_batch_rows`. The rows add up, every value is correct, and it is still not a
    /// substitute for the module: the list of batches is what a caller gets back, and a caller that
    /// asked for ten million rows in pieces it can start working on has been handed something else.
    #[derive(Debug)]
    struct OneBatch;

    impl Native for OneBatch {
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
                let ignored = Hello {
                    max_batch_rows: u64::MAX,
                    ..*hello
                };
                fixedwidth_batches(&ignored, request, source).await
            })
        }
    }

    let err = failure(Arc::new(OneBatch));
    assert!(
        matches!(err, Mismatch::Batches { native: 1, .. }),
        "this is the one a run at a single batch size would have passed: {err}"
    );
}

#[test]
fn an_implementation_that_describes_itself_differently_is_caught() {
    /// Answers the handshake with a decoder id of its own.
    ///
    /// The handshake is what the host negotiates against, so an implementation that answers it
    /// differently is a different decoder before a row has been read. This is checked ahead of the
    /// rows because a mismatch here explains every mismatch that would follow it.
    #[derive(Debug)]
    struct Renamed;

    impl Native for Renamed {
        fn handshake(&self, _hello: &Hello) -> Result<Handshake> {
            Ok(Handshake {
                decoder_id: "fixedwidth2".to_owned(),
                ..fixedwidth_handshake()
            })
        }

        fn scan<'a>(
            &'a self,
            hello: &'a Hello,
            request: &'a ScanRequest<'a>,
            source: &'a mut (dyn RangeSource + Send),
        ) -> Scanning<'a> {
            Box::pin(fixedwidth_batches(hello, request, source))
        }
    }

    let err = failure(Arc::new(Renamed));
    assert!(matches!(err, Mismatch::Handshake { .. }), "{err}");
}

#[test]
fn an_implementation_that_refuses_what_the_module_serves_is_caught() {
    /// Will not serve a request for no rows.
    ///
    /// An empty request is legal and the module answers it with no batches at all, which is the sort
    /// of edge an implementation written from a description of the format rather than from the module
    /// gets wrong. Refusing where the module serves is a disagreement even though no bytes differ,
    /// because the caller gets an error instead of an answer.
    #[derive(Debug)]
    struct Fussy;

    impl Native for Fussy {
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
                if request.row_count == 0 {
                    return Err(iris_native::Error::malformed("ask for at least one row"));
                }
                fixedwidth_batches(hello, request, source).await
            })
        }
    }

    let err = failure(Arc::new(Fussy));
    assert!(matches!(err, Mismatch::Outcome { .. }), "{err}");
}

#[test]
fn a_run_over_an_empty_corpus_is_not_a_run() {
    let err = attempt(Arc::new(FixedWidth), &Corpus::new(), &both())
        .expect_err("a run with nothing to compare proves nothing and says so");
    assert!(matches!(err, Mismatch::EmptyCorpus), "{err}");
}

#[test]
fn a_run_with_no_terms_is_not_a_run() {
    let err = attempt(Arc::new(FixedWidth), &native_corpus(), &[])
        .expect_err("terms are what a decoder is run under, so a run needs some");
    assert!(matches!(err, Mismatch::NoTerms), "{err}");
}

#[test]
fn a_case_with_no_rows_is_not_a_case() {
    let corpus = Corpus::new().with_case(Case::new("nothing at all", source(0, 1), 0, 1));
    let err = attempt(Arc::new(FixedWidth), &corpus, &both())
        .expect_err("a case with no rows has nothing to compare on it");
    assert!(matches!(err, Mismatch::EmptyCase { .. }), "{err}");
}
