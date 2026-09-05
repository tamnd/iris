//! Every implementation of `RangeSource`, through the same suite.
//!
//! `tamnd/iris` #22 asks for three implementations that all pass one conformance suite, and this is
//! the file that makes that claim checkable. If a check here only passes for one of them then either
//! the check is testing an implementation detail or the trait does not mean what it says, and both
//! of those are worth finding out before a decoder depends on it.
//!
//! The corpus is arithmetic rather than a fixture, for the same reason the window stress test is: a
//! byte's value is a function of its own offset, so a byte read from the wrong place is wrong on its
//! own without needing to know where it came from. Bytes that all look alike would let a source that
//! is off by a page pass everything.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the corpus pattern is a byte by construction"
)]

use std::fs::File;
use std::io::Write as _;

use iris_source::{FileSource, MemorySource, Readahead, conformance};

/// Large enough to cross several pages and to make a small window slide, small enough that the
/// suite is a fraction of a second.
const CORPUS: usize = 300_000;

/// A window span that forces slides rather than mapping the whole corpus once.
///
/// The point of running the file source through the suite is the slide, so a span that happened to
/// cover everything would be testing the trait and not the implementation.
const SPAN: usize = 64 * 1024;

/// Bytes whose value says where they came from.
fn corpus(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at % 251) as u8).collect()
}

/// The corpus written to a temporary file, kept alive by the returned handle.
fn corpus_file(contents: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("a temporary file");
    file.write_all(contents).expect("writing the corpus");
    file.flush().expect("flushing the corpus");
    file
}

#[test]
fn a_memory_source_is_conformant() {
    let contents = corpus(CORPUS);
    let mut source = MemorySource::new(contents.clone());
    conformance::check(&mut source, &contents);
}

#[test]
fn a_file_source_is_conformant_through_a_window_that_has_to_slide() {
    let contents = corpus(CORPUS);
    let file = corpus_file(&contents);

    let mut source =
        FileSource::with_span(File::open(file.path()).expect("reopening the corpus"), SPAN)
            .expect("a window over the corpus");
    conformance::check(&mut source, &contents);

    assert!(
        source.slides() > 0,
        "a corpus of {CORPUS} bytes through a {SPAN} byte window should have moved the view"
    );
}

#[test]
fn a_file_source_over_an_empty_file_is_conformant() {
    // The empty file has no section to map and no view, which is a different path through every
    // method and the one an arithmetic corpus never reaches.
    let file = corpus_file(&[]);
    let mut source = FileSource::with_span(
        File::open(file.path()).expect("reopening the empty file"),
        SPAN,
    )
    .expect("a window over an empty file");
    conformance::check(&mut source, &[]);
}

/// Readahead in front of a source it can actually help.
///
/// The interesting one, because reading ahead changes which ranges are asked for underneath and the
/// promises are about the ranges that come back. A block that is off by one, or a block that is
/// reused for a range it does not cover, is a source that answers with the wrong bytes, and the
/// arithmetic corpus is what turns that into a failed comparison rather than a plausible answer.
#[test]
fn reading_ahead_of_a_window_that_has_to_slide_is_conformant() {
    let contents = corpus(CORPUS);
    let file = corpus_file(&contents);

    let inner = FileSource::with_span(File::open(file.path()).expect("reopening the corpus"), SPAN)
        .expect("a window over the corpus");
    // Deeper than the suite's largest request and shallower than the window, so blocks are real and
    // the source underneath can still serve them.
    let mut source = Readahead::new(inner, SPAN / 4);
    conformance::check(&mut source, &contents);
}

#[test]
fn reading_ahead_of_nothing_is_conformant() {
    // An empty source has nothing ahead of anything, which is the case where a block length would
    // come out zero if it were calculated rather than short circuited.
    let mut source = Readahead::new(MemorySource::new(Vec::new()), 4096);
    conformance::check(&mut source, &[]);
}

#[test]
fn a_memory_source_over_nothing_is_conformant() {
    let mut source = MemorySource::new(Vec::new());
    conformance::check(&mut source, &[]);
}

/// The object store source, which is the only one that is ever pending.
///
/// It runs against the in memory store rather than a real endpoint, because what is being checked
/// here is the trait and not the network. Whether the same decoder reads from a local file and from
/// an S3 compatible endpoint is `tamnd/iris` #25, and that one wants a real server.
#[cfg(feature = "object-store")]
mod object {
    use std::sync::Arc;

    use bytes::Bytes;
    use iris_source::{ObjectSource, RangeSource, Traffic, conformance, read_blocking};
    use object_store::{ObjectStore, ObjectStoreExt as _, memory::InMemory, path::Path};

    use super::{CORPUS, corpus};

    /// A runtime with a worker thread, a store holding the corpus, and a source over it.
    ///
    /// The runtime has to be a multi threaded one. A current thread runtime only runs a spawned
    /// task while somebody awaits on it, and nobody here ever does, so every request would sit
    /// pending forever and the suite would hang rather than fail. That is not a defect in the
    /// source: it is what "the host supplies the runtime" means, and a host that spawns work onto a
    /// runtime it never drives has not supplied one.
    fn source() -> (tokio::runtime::Runtime, ObjectSource, Vec<u8>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("a runtime");

        let contents = corpus(CORPUS);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("corpus");

        let put = store.clone();
        let payload = Bytes::from(contents.clone());
        let at = path.clone();
        runtime
            .block_on(async move { put.put(&at, payload.into()).await })
            .expect("putting the corpus");

        let source = runtime
            .block_on(ObjectSource::open(Arc::clone(&store), path))
            .expect("opening the object");

        (runtime, source, contents)
    }

    #[test]
    fn an_object_source_is_conformant() {
        let (_runtime, mut source, contents) = source();
        conformance::check(&mut source, &contents);
    }

    #[test]
    fn an_object_source_says_pending_before_it_says_ready() {
        let (_runtime, mut source, contents) = source();

        // The first ask for a range cannot be ready, because nothing has been requested yet. This
        // is the property the whole resumable path depends on and it is the one thing the other two
        // implementations cannot demonstrate.
        assert!(
            !source.range(0, 64).expect("in bounds").is_ready(),
            "the first ask for a range that has not been fetched has to be pending"
        );

        let bytes = read_blocking(&mut source, 0, 64).expect("the bytes arrive");
        assert_eq!(bytes, &contents[..64]);
    }

    #[test]
    fn an_object_source_counts_what_it_sent_and_what_came_back() {
        let (_runtime, mut source, contents) = source();

        read_blocking(&mut source, 0, 64).expect("the bytes arrive");
        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 1,
                bytes: 64
            },
            "one range is one request and sixty four bytes"
        );

        // A range inside the block already held costs nothing, which is the only reason keeping a
        // block is worth doing at all.
        read_blocking(&mut source, 8, 8).expect("already held");
        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 1,
                bytes: 64
            },
            "a range inside the held block is free"
        );

        // One outside it costs a request.
        let far = contents.len() as u64 - 32;
        read_blocking(&mut source, far, 32).expect("the tail arrives");
        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 2,
                bytes: 96
            }
        );
    }

    #[test]
    fn an_object_source_serves_an_empty_range_without_going_to_the_store() {
        let (_runtime, mut source, contents) = source();

        let bytes = read_blocking(&mut source, contents.len() as u64, 0).expect("empty is fine");
        assert!(bytes.is_empty());
        assert_eq!(
            source.traffic(),
            Traffic::NONE,
            "no bytes wanted is no round trip worth making"
        );
    }
}
