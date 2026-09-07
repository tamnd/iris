//! Every scan says which of the two implementations ran it, and under what identities.
//!
//! Substitution is meant to be invisible in the answers. That is the entry requirement: a native
//! implementation only reaches a registry by producing what the module produces, byte for byte, so
//! nothing in a batch tells you which side made it. Which is fine right up until a number is wrong,
//! and then the first question is which side made it, and there is nowhere to look.
//!
//! So a scan writes one line about itself. These tests are that line existing, on both open paths,
//! for a scan that worked and for one that did not, carrying the digest of the decoder the container
//! shipped and the digest of the run that let the substitute stand in for it.
//!
//! They subscribe to it the way anything subscribes to it, which is worth saying because it is the
//! only thing this crate is not allowed to do for itself. A library that installed a collector would
//! be deciding where an embedder's logs go. It records events and stops there, and one of these
//! tests is the proof that the events are real rather than a comment about intent.

mod support;

use std::io;
use std::sync::{Arc, Mutex};

use iris_runtime::{Registry, Runtime};
use iris_source::MemorySource;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

use support::{FIXEDWIDTH_IDENTITY, FixedWidth, builder, prove};

/// Small, because none of this is about how long a scan takes.
const ROWS: u64 = 512;

/// Two, so a projection has something to leave out.
const COLUMNS: u64 = 2;

/// The ordinary fixture: the fixed width module and the data it reads.
fn container() -> Vec<u8> {
    builder(ROWS, COLUMNS)
        .build()
        .expect("the container is writable")
}

/// Somewhere for a collector to write that a test can read afterwards.
///
/// Behind a mutex because `MakeWriter` hands out a writer per event and this has to survive being
/// handed out more than once. Cloning shares the buffer rather than copying it, which is the whole
/// arrangement: the collector holds one clone and the test holds another.
#[derive(Clone, Debug, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// What has been written so far, as text.
    fn text(&self) -> String {
        let held = self.0.lock().expect("no test panics while holding this");
        String::from_utf8(held.clone()).expect("a formatted event is text")
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("no test panics while holding this")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Captured {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// Runs something with a collector attached, and hands back everything it logged.
///
/// The collector is installed for this thread and for the duration of the call rather than for the
/// process, which is what `with_default` is for. Tests run in parallel in one process and a global
/// collector can be installed once, so a test that installed one would be a test that only works
/// when it happens to run first.
///
/// Time and colour are off so what comes back is stable enough to assert on.
fn capture<T>(run: impl FnOnce() -> T) -> (T, String) {
    let sink = Captured::default();
    let collector = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .finish();
    let answer = tracing::subscriber::with_default(collector, run);
    let text = sink.text();
    (answer, text)
}

#[test]
fn a_scan_in_the_sandbox_says_so_and_names_the_decoder() {
    let bytes = container();
    let runtime = Runtime::new().expect("a runtime starts");

    let (digest, log) = capture(|| {
        let dataset = runtime.open(&bytes).expect("the container opens");
        dataset.scan().expect("the module reads its own fixture");
        dataset.decoder_digest()
    });

    assert!(log.contains("path=\"sandbox\""), "{log}");
    assert!(log.contains(&format!("decoder={digest}")), "{log}");
    assert!(log.contains("row_start=0"), "{log}");
    assert!(log.contains(&format!("row_count={ROWS}")), "{log}");

    // Absent rather than empty, and that is the answer rather than a gap in it. There is no kernel
    // on this path because nothing stood in for anything, so there is no value that would be honest
    // to print here.
    assert!(!log.contains("kernel="), "{log}");
    assert!(!log.contains("implementation="), "{log}");
    assert!(!log.contains("failed="), "{log}");
}

#[test]
fn a_substituted_scan_names_the_kernel_and_what_it_calls_itself() {
    let bytes = container();
    let kernel = prove(Arc::new(FixedWidth));

    // Read before the kernel goes into the registry, because this is the value the host is supposed
    // to write down and a test that read it back out of the log it is checking would prove nothing.
    let proof = kernel.proof();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(Registry::new().with(kernel));

    let (digest, log) = capture(|| {
        let dataset = runtime.open(&bytes).expect("the container opens");
        assert!(dataset.decoder_is_native(), "the digest is registered");
        dataset.scan().expect("the kernel reads the same fixture");
        dataset.decoder_digest()
    });

    assert!(log.contains("path=\"native\""), "{log}");

    // Three identities and they answer three different questions. The decoder digest is what the
    // container shipped, the kernel digest is which differential run admitted the substitute, and
    // the last one is what an operator has to go and look at.
    assert!(log.contains(&format!("decoder={digest}")), "{log}");
    assert!(log.contains(&format!("kernel={proof}")), "{log}");
    assert!(
        log.contains(&format!("implementation=\"{FIXEDWIDTH_IDENTITY}\"")),
        "{log}"
    );
    assert_ne!(
        digest.to_string(),
        proof.to_string(),
        "a kernel is not the module it stands in for"
    );
}

#[test]
fn the_windowed_path_writes_the_same_line() {
    let bytes = container();
    let kernel = prove(Arc::new(FixedWidth));
    let proof = kernel.proof();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(Registry::new().with(kernel));

    let (digest, log) = capture(|| {
        let mut dataset = runtime
            .open_windowed(Box::new(MemorySource::new(bytes.clone())))
            .expect("the container opens");
        dataset.scan().expect("the kernel reads the same fixture");
        dataset.decoder_digest()
    });

    assert!(log.contains("path=\"native\""), "{log}");
    assert!(log.contains(&format!("decoder={digest}")), "{log}");
    assert!(log.contains(&format!("kernel={proof}")), "{log}");
}

#[test]
fn a_scan_that_failed_is_still_a_scan_that_ran_somewhere() {
    let bytes = container();
    let runtime = Runtime::new()
        .expect("a runtime starts")
        .with_native(Registry::new().with(prove(Arc::new(FixedWidth))));

    let ((), log) = capture(|| {
        let dataset = runtime.open(&bytes).expect("the container opens");
        let one_past_the_end = u32::try_from(COLUMNS).expect("two columns");
        dataset
            .scan_columns(&[one_past_the_end])
            .expect_err("column two of a two column dataset is not a column");
    });

    // The failure and the path are in the same event on purpose. A scan that went wrong is exactly
    // the case where which of the two implementations ran it is the question, so splitting them
    // across two lines would be splitting them at the worst possible moment.
    assert!(log.contains("path=\"native\""), "{log}");
    assert!(log.contains("failed="), "{log}");
    assert!(log.contains("names column 2"), "{log}");
}

#[test]
fn a_projection_shows_up_as_the_columns_that_were_asked_for() {
    let bytes = container();
    let runtime = Runtime::new().expect("a runtime starts");

    let ((), log) = capture(|| {
        let dataset = runtime.open(&bytes).expect("the container opens");
        dataset
            .scan_columns(&[1])
            .expect("the second column exists");
    });

    assert!(log.contains("columns=[1]"), "{log}");
}
