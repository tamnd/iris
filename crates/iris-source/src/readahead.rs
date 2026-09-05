//! Reading more than was asked for, so that the next ask is already answered.
//!
//! A decoder that walks a column asks for it in the pieces the encoding is written in, and those
//! pieces are small and next to each other. Served one at a time against an object store that is
//! forty small requests and forty round trips, and the round trip is the whole cost. Served through
//! this it is one request, because the first ask fetches a block and the thirty nine after it are
//! comparisons.
//!
//! Coalescing without reading ahead is not possible from here. Merging forty requests into one means
//! knowing that the other thirty nine are coming, and the only thing that knows is the decoder, which
//! is the one part of the system that is deliberately not allowed to make I/O decisions. So the host
//! guesses instead, and the guess is that a scan reads forwards. That is a guess a host is entitled
//! to make and a decoder is not, which is the same reason the rest of the I/O policy lives on this
//! side of the boundary.
//!
//! # Why more than one block
//!
//! One block would be enough if a scan read a container from front to back, and a columnar scan does
//! not. It reads a piece of the first column, then a piece of the second, then a piece of the third,
//! then the next piece of the first, and those three places are megabytes apart. A single block is
//! evicted by every one of those turns and the hit rate is zero, which is worse than not reading
//! ahead at all, because each miss now fetches a whole block to serve one piece of it.
//!
//! So this keeps a few blocks and replaces the one that has gone longest without being read. Three
//! columns read in turn is three runs moving forwards, and three blocks hold all three. The number is
//! [`Readahead::with_streams`], and the honest way to pick it is the number of columns a query
//! touches.
//!
//! # Forwards only
//!
//! A block starts where the request started and runs forwards. It would have been slightly simpler to
//! align blocks to a fixed grid, and then a request near the start of a block would drag in the bytes
//! before it, which for a footer read at the end of a container is the whole block spent on data that
//! is behind where anything will look. Starting at the request costs an overlapping re-read when one
//! request straddles the end of the block already held, and that happens once per block rather than
//! once per request.
//!
//! # The depth is the host's
//!
//! [`Readahead::new`] takes the depth and there is no way for a decoder to influence it, ask what it
//! is, or find out that there is one. That is deliberate rather than incidental: how far ahead to
//! read depends on what the bytes are behind, which the host knows and the decoder does not. A
//! sensible depth for an object over a network is megabytes, for a local file it is what the page
//! cache is doing anyway, and a decoder that could demand either one would be making the decision
//! from the one place in the system with the least information about it.
//!
//! # What it costs
//!
//! A block is copied out of the source underneath and held here, so this holds `depth * streams`
//! bytes and does one copy per block fetched rather than per range served. That trade is worth making
//! when a request is a round trip and is not worth making when it is not. A mapped local file already
//! coalesces, because a window slide is the request and everything inside the current view is a
//! comparison, and putting this in front of one buys a copy and nothing else.
//!
//! Requests as long as the depth or longer go straight through and are not held. Nothing is gained by
//! reading ahead of a request that is already the size of a block, and copying one would be the
//! largest cost here for the least reason.

use crate::source::{Fetch, RangeSource, SourceError, Traffic, bounds};

/// A source that fetches in blocks, so that adjacent requests become one request.
///
/// See the [module documentation](self) for what the depth means, why there is more than one block,
/// and why both are the host's to pick.
#[derive(Clone, Debug)]
pub struct Readahead<S> {
    inner: S,
    depth: usize,
    streams: usize,
    /// The blocks held, most recently read first.
    ///
    /// A list rather than a map because it holds a handful of entries and is scanned linearly, and
    /// because the order is the replacement policy. Nothing here would survive being asked to hold
    /// thousands, which is a cache and a different thing to build.
    blocks: Vec<Block>,
}

/// One block of the source, and where it came from.
#[derive(Clone, Debug)]
struct Block {
    at: u64,
    bytes: Vec<u8>,
}

/// What a request turned out to need, worked out before anything is borrowed to answer it.
///
/// The fetching and the answering are separate steps because they cannot both hold the source at
/// once: the answer is a slice of a block and the fetch is a call on the source underneath, so
/// deciding first and borrowing second is what keeps those apart.
enum Plan {
    /// No bytes wanted.
    Empty,
    /// Longer than a block, so it is the source underneath's to serve directly.
    Through,
    /// Not here yet.
    Pending,
    /// Held, at this index.
    Held(usize),
}

impl<S: RangeSource> Readahead<S> {
    /// Reads `depth` bytes at a time from `inner`, keeping one block.
    ///
    /// A depth of zero is the way to say no readahead at all, and it makes this a pass through
    /// rather than an error. See [`Readahead::with_streams`] for reading more than one run at once,
    /// which is what a columnar scan needs.
    #[must_use]
    pub fn new(inner: S, depth: usize) -> Self {
        Self {
            inner,
            depth,
            streams: 1,
            blocks: Vec::new(),
        }
    }

    /// Keeps `streams` blocks rather than one.
    ///
    /// The number of places in the source that are being read forwards at the same time, which for a
    /// columnar scan is the number of columns it touches. Below that, each column evicts the block
    /// the next one is about to want and nothing is ever hit. Above it, the extra blocks are memory
    /// that is held and not read.
    ///
    /// Zero means the same as one, because a source that holds no blocks would fetch one per request
    /// and throw it away, which is every cost of this type and none of the benefit.
    #[must_use]
    pub fn with_streams(mut self, streams: usize) -> Self {
        self.streams = streams.max(1);
        self.blocks.truncate(self.streams);
        self
    }

    /// How far ahead this reads.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// How many blocks it holds at once.
    #[must_use]
    pub fn streams(&self) -> usize {
        self.streams
    }

    /// The source underneath, for a host that wants to ask it something directly.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The source underneath, handed back.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Which held block covers a range, if any.
    fn find(&self, at: u64, end: u64) -> Option<usize> {
        self.blocks.iter().position(|block| {
            at >= block.at && end <= block.at.saturating_add(block.bytes.len() as u64)
        })
    }

    /// How long a block to ask for, given a request that has already been bounds checked.
    ///
    /// Three things bound it. The end of the source, because reading ahead past it is not a range.
    /// [`RangeSource::largest`], because a windowed source underneath refuses a block that does not
    /// fit in one view, and finding that out as a refusal the caller has to handle would make this
    /// adapter something a host has to think about rather than something it can put in the way. And
    /// the request itself, from below, so that a source with a small enough `largest` reports its own
    /// refusal with its own numbers instead of this one inventing a smaller request that succeeds.
    fn block_for(&self, at: u64, len: usize) -> usize {
        let remaining = self.inner.len() - at;
        let ahead = usize::try_from(u64::try_from(self.depth).unwrap_or(u64::MAX).min(remaining))
            .unwrap_or(usize::MAX);
        let block = match self.inner.largest() {
            Some(largest) => ahead.min(largest),
            None => ahead,
        };
        block.max(len)
    }

    /// Works out how to answer a request, fetching a block if there is not one that covers it.
    fn prepare(&mut self, at: u64, len: usize, end: u64) -> Result<Plan, SourceError> {
        if len == 0 {
            return Ok(Plan::Empty);
        }
        if len >= self.depth {
            return Ok(Plan::Through);
        }

        if let Some(index) = self.find(at, end) {
            // Read, so it goes to the front and something else becomes the one to replace.
            let block = self.blocks.remove(index);
            self.blocks.insert(0, block);
            return Ok(Plan::Held(0));
        }

        let block_len = self.block_for(at, len);
        let bytes = match self.inner.range(at, block_len)? {
            // Copied here rather than held as a borrow, which is the cost this type pays for
            // holding more than one block: the source underneath keeps at most whatever it last
            // fetched, so a second block can only exist on this side.
            Fetch::Ready(bytes) => bytes.to_vec(),
            Fetch::Pending => return Ok(Plan::Pending),
        };

        if self.blocks.len() >= self.streams {
            self.blocks.truncate(self.streams.saturating_sub(1));
        }
        self.blocks.insert(0, Block { at, bytes });
        Ok(Plan::Held(0))
    }
}

impl<S: RangeSource> RangeSource for Readahead<S> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn largest(&self) -> Option<usize> {
        // Unchanged, because a block is never shorter than the range that caused it and never
        // longer than one call underneath can serve. Anything this refused would have been refused
        // there, and with better numbers in the message.
        self.inner.largest()
    }

    fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
        let end = bounds(at, len, self.inner.len())?;

        match self.prepare(at, len, end)? {
            Plan::Empty => Ok(Fetch::Ready(&[])),
            Plan::Through => self.inner.range(at, len),
            Plan::Pending => Ok(Fetch::Pending),
            Plan::Held(index) => {
                let block = &self.blocks[index];
                // Inside the block by the check that found it, so the difference is smaller than
                // the block and fits.
                let offset = usize::try_from(at - block.at).unwrap_or(usize::MAX);
                block
                    .bytes
                    .get(offset..offset + len)
                    .map(Fetch::Ready)
                    .ok_or_else(|| SourceError::Fetch {
                        at,
                        end,
                        // A source that answers Ready with fewer bytes than it was asked for has
                        // broken the one promise the enum exists to make. Saying so is better than
                        // indexing and turning it into a panic somewhere with no context.
                        source: "the source underneath answered a block short".into(),
                    })
            }
        }
    }

    fn traffic(&self) -> Traffic {
        // The source underneath is the one that does the fetching, and its numbers are the ones
        // worth reporting. This adapter exists to make them smaller, so a count of its own that
        // included the requests it absorbed would hide exactly the thing it is for.
        self.inner.traffic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemorySource;

    /// A source that counts the trips it makes, which is the only thing worth checking here.
    ///
    /// Wrapping a memory source would prove nothing, because a memory source reports no traffic and
    /// this adapter reports whatever is underneath it. So the thing underneath has to be something
    /// that says how many times it went and fetched.
    #[derive(Debug)]
    struct Counted {
        inner: MemorySource,
        requests: u64,
        bytes: u64,
    }

    impl Counted {
        fn new(contents: Vec<u8>) -> Self {
            Self {
                inner: MemorySource::new(contents),
                requests: 0,
                bytes: 0,
            }
        }
    }

    impl RangeSource for Counted {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
            self.requests += 1;
            self.bytes += len as u64;
            self.inner.range(at, len)
        }

        fn traffic(&self) -> Traffic {
            Traffic {
                requests: self.requests,
                bytes: self.bytes,
            }
        }
    }

    fn corpus(len: usize) -> Vec<u8> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the pattern is a byte by construction"
        )]
        (0..len).map(|at| (at % 251) as u8).collect()
    }

    /// The bytes a range came back with, or a panic naming what happened instead.
    ///
    /// Offsets are `usize` here because every one of them is also an index into the corpus, and a
    /// test that converts back and forth between the two is a test about conversions.
    fn read(source: &mut Readahead<Counted>, at: usize, len: usize) -> Vec<u8> {
        let offset = u64::try_from(at).expect("a corpus this size fits in a u64");
        match source.range(offset, len) {
            Ok(Fetch::Ready(bytes)) => bytes.to_vec(),
            other => panic!("a resident source is never pending: {other:?}"),
        }
    }

    #[test]
    fn forty_adjacent_requests_become_one() {
        let contents = corpus(64 * 1024);
        let mut source = Readahead::new(Counted::new(contents.clone()), 64 * 1024);

        for step in 0..40 {
            let at = step * 1024;
            assert_eq!(read(&mut source, at, 1024), contents[at..at + 1024]);
        }

        assert_eq!(
            source.traffic().requests,
            1,
            "forty adjacent requests inside one block are one request underneath"
        );
    }

    #[test]
    fn three_columns_read_in_turn_need_three_blocks() {
        // The access pattern a columnar scan actually makes: a piece of each column, then the next
        // piece of each column, with the columns far enough apart that no block covers two.
        const COLUMN: usize = 256 * 1024;
        const PIECE: usize = 4 * 1024;

        let contents = corpus(3 * COLUMN);
        let pattern = |source: &mut Readahead<Counted>| {
            for step in 0..16 {
                for column in 0..3 {
                    let at = column * COLUMN + step * PIECE;
                    assert_eq!(read(source, at, PIECE), contents[at..at + PIECE]);
                }
            }
        };

        let mut one = Readahead::new(Counted::new(contents.clone()), 32 * 1024);
        pattern(&mut one);
        assert_eq!(
            one.traffic().requests,
            48,
            "one block is evicted by every column change, so nothing is ever hit"
        );

        let mut three = Readahead::new(Counted::new(contents.clone()), 32 * 1024).with_streams(3);
        pattern(&mut three);
        assert_eq!(
            three.traffic().requests,
            6,
            "sixty four kilobytes per column in thirty two kilobyte blocks is two each"
        );
    }

    #[test]
    fn a_depth_of_nothing_passes_every_request_through() {
        let contents = corpus(4096);
        let mut source = Readahead::new(Counted::new(contents), 0);

        for step in 0..4u64 {
            assert!(source.range(step * 64, 64).expect("in bounds").is_ready());
        }

        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 4,
                bytes: 256
            },
            "no readahead is one request each and not a byte more than was asked for"
        );
    }

    #[test]
    fn a_request_as_long_as_a_block_is_not_worth_holding() {
        let contents = corpus(4096);
        let mut source = Readahead::new(Counted::new(contents), 1024);

        assert_eq!(read(&mut source, 0, 1024).len(), 1024);
        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 1,
                bytes: 1024
            },
            "a request the size of a block is served as itself and not read ahead of"
        );

        // And nothing was kept, so the same request again is a second trip rather than a hit.
        assert_eq!(read(&mut source, 0, 1024).len(), 1024);
        assert_eq!(source.traffic().requests, 2);
    }

    #[test]
    fn a_request_that_straddles_the_block_starts_a_new_one() {
        let contents = corpus(4096);
        let mut source = Readahead::new(Counted::new(contents), 1024);

        assert_eq!(read(&mut source, 0, 16).len(), 16);
        assert_eq!(source.traffic().requests, 1);

        // Ends one byte past the block, so it cannot be served out of it.
        assert_eq!(read(&mut source, 1020, 8).len(), 8);
        assert_eq!(source.traffic().requests, 2);

        // And the new block starts where that request did, so what follows it is free.
        assert_eq!(read(&mut source, 1028, 8).len(), 8);
        assert_eq!(source.traffic().requests, 2);
    }

    #[test]
    fn reading_ahead_stops_at_the_end_of_the_source() {
        let contents = corpus(100);
        let mut source = Readahead::new(Counted::new(contents), 4096);

        assert_eq!(read(&mut source, 90, 10).len(), 10);
        assert_eq!(
            source.traffic(),
            Traffic {
                requests: 1,
                bytes: 10
            },
            "a block runs to the end of the source and no further"
        );
    }

    #[test]
    fn a_block_never_grows_past_what_one_call_underneath_can_serve() {
        /// A source that refuses anything longer than a quarter of what a block would want.
        #[derive(Debug)]
        struct Bounded(MemorySource);

        impl RangeSource for Bounded {
            fn len(&self) -> u64 {
                self.0.len()
            }

            fn largest(&self) -> Option<usize> {
                Some(256)
            }

            fn range(&mut self, at: u64, len: usize) -> Result<Fetch<'_>, SourceError> {
                if len > 256 {
                    return Err(SourceError::TooLarge {
                        wanted: len,
                        largest: 256,
                    });
                }
                self.0.range(at, len)
            }

            fn traffic(&self) -> Traffic {
                Traffic::NONE
            }
        }

        let mut source = Readahead::new(Bounded(MemorySource::new(corpus(4096))), 4096);
        assert!(source.range(0, 16).expect("in bounds").is_ready());

        // And a request the source underneath could not serve either is still its refusal, with its
        // own numbers in it, rather than something this adapter invented.
        assert!(matches!(
            source.range(0, 512),
            Err(SourceError::TooLarge {
                wanted: 512,
                largest: 256
            })
        ));
    }

    #[test]
    fn an_empty_range_at_the_end_asks_for_nothing() {
        let mut source = Readahead::new(Counted::new(corpus(64)), 4096);

        assert!(matches!(source.range(64, 0), Ok(Fetch::Ready([]))));
        assert_eq!(source.traffic(), Traffic::NONE);
    }

    #[test]
    fn asking_for_no_streams_still_holds_a_block() {
        let contents = corpus(4096);
        let mut source = Readahead::new(Counted::new(contents), 1024).with_streams(0);

        assert_eq!(source.streams(), 1);
        assert_eq!(read(&mut source, 0, 16).len(), 16);
        assert_eq!(read(&mut source, 16, 16).len(), 16);
        assert_eq!(source.traffic().requests, 1);
    }
}
