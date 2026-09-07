//! Opening a container, reading a range of rows out of it, and letting go again.
//!
//! # Why a partition opens the dataset rather than sharing one
//!
//! A source is a position as well as a place. `iris_runtime::Windowed` scans through `&mut self`
//! for that reason: reading a range moves a window, drops a block and counts a request, and two
//! partitions doing that to one source would be two scans reading each other's window. So each
//! partition opens what it needs, reads its own rows, and closes it, and nothing is shared between
//! them except bytes nobody writes to.
//!
//! `iris_runtime::Dataset` would allow sharing, because the resident path scans through a shared
//! reference. It is not shared here anyway, and the reason is a lifetime rather than a preference:
//! a `Dataset` borrows the buffer it was opened from, so holding one in a table provider means
//! holding a borrow of something the same struct owns. There is a way to write that and it needs
//! unsafe code. Opening twice does not, and the two paths staying the same shape is worth more than
//! the open this saves.
//!
//! What that costs is one module compilation per partition, because opening a container compiles
//! the decoder in it. That is the honest cost of this design today and it is what iris #127, the
//! compiled module cache, is for. When a `Runtime` remembers modules it has already compiled, this
//! becomes an open of the metadata and nothing else, with no change here.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use iris_runtime::{CapabilitySet, Digest, Runtime, Traffic};
use iris_source::FileSource;

use crate::error::Error;

/// What a table needs to know about a container before it can answer a planner.
#[derive(Debug)]
pub(crate) struct Meta {
    /// The Arrow schema the container carries.
    pub(crate) schema: SchemaRef,
    /// How many rows it says it has.
    pub(crate) rows: u64,
    /// What the container calls itself, which is not what the table is registered as.
    pub(crate) name: String,
    /// The hash of the decoder that will run, checked before anything compiled it.
    pub(crate) digest: Digest,
    /// What the host and the decoder settled on, which is what says whether a projection may be
    /// pushed through rather than applied here.
    pub(crate) agreed: CapabilitySet,
}

/// What one partition got, and what it cost to get it.
#[derive(Debug)]
pub(crate) struct Rows {
    /// The batches, carrying the projected schema when a projection was asked for.
    pub(crate) batches: Vec<RecordBatch>,
    /// What the scan asked of the source, which is zero on the resident path.
    pub(crate) traffic: Traffic,
}

/// A container this table can open, as many times as there are partitions.
///
/// `Send` and `Sync` because a table provider is shared between the threads an engine runs a query
/// on, and every partition calls [`Open::rows`] through the same shared reference.
pub(crate) trait Open: fmt::Debug + Send + Sync {
    /// Opens the container and reads what a planner needs, without reading any data.
    fn meta(&self) -> Result<Meta, Error>;

    /// Reads `count` rows from `start`, of the columns named, or of all of them if none are.
    fn rows(&self, start: u64, count: u64, columns: &[u32]) -> Result<Rows, Error>;
}

/// A container that is already in memory.
pub(crate) struct Resident {
    runtime: Runtime,
    bytes: Arc<[u8]>,
}

impl Resident {
    pub(crate) const fn new(runtime: Runtime, bytes: Arc<[u8]>) -> Self {
        Self { runtime, bytes }
    }
}

impl Open for Resident {
    fn meta(&self) -> Result<Meta, Error> {
        let dataset = self.runtime.open(&self.bytes)?;
        Ok(Meta {
            schema: Arc::clone(dataset.schema()),
            rows: dataset.rows(),
            name: dataset.name().to_owned(),
            digest: dataset.decoder_digest(),
            agreed: dataset.capabilities()?,
        })
    }

    fn rows(&self, start: u64, count: u64, columns: &[u32]) -> Result<Rows, Error> {
        let dataset = self.runtime.open(&self.bytes)?;
        Ok(Rows {
            batches: fill(count, |at, left| {
                Ok(dataset.scan_rows_columns(start + at, left, columns)?)
            })?,
            // Always nothing, and that is the honest answer rather than a gap. Whoever produced
            // this buffer paid for every byte of it before this table existed, whether or not the
            // query went on to read a hundredth of it.
            traffic: dataset.last_scan(),
        })
    }
}

// Written out rather than derived, because a reader looking at a plan wants to know how big the
// buffer is and not what is in it.
impl fmt::Debug for Resident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resident")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// A container that stays in a file, read one range at a time through a window.
#[derive(Debug)]
pub(crate) struct Stored {
    runtime: Runtime,
    path: PathBuf,
}

impl Stored {
    pub(crate) const fn new(runtime: Runtime, path: PathBuf) -> Self {
        Self { runtime, path }
    }

    /// Opens the file and hands the runtime a source over it.
    fn windowed(&self) -> Result<iris_runtime::Windowed, Error> {
        let source = FileSource::open(&self.path)?;
        Ok(self.runtime.open_windowed(Box::new(source))?)
    }
}

impl Open for Stored {
    fn meta(&self) -> Result<Meta, Error> {
        let mut dataset = self.windowed()?;
        Ok(Meta {
            schema: Arc::clone(dataset.schema()),
            rows: dataset.rows(),
            name: dataset.name().to_owned(),
            digest: dataset.decoder_digest(),
            agreed: dataset.capabilities()?,
        })
    }

    fn rows(&self, start: u64, count: u64, columns: &[u32]) -> Result<Rows, Error> {
        let mut dataset = self.windowed()?;

        // Read either side of the scanning rather than taken from `last_scan`, because the range
        // may take more than one scan and `last_scan` is the last of them. What is subtracted off
        // is opening the container, which costs the same reads whatever the query was, so folding
        // it in would add a constant to every number and make two queries look more alike than
        // they are.
        let before = dataset.traffic();
        let batches = fill(count, |at, left| {
            Ok(dataset.scan_rows_columns(start + at, left, columns)?)
        })?;
        Ok(Rows {
            batches,
            traffic: dataset.traffic().since(before),
        })
    }
}

/// Reads a whole range of rows, however many scans that takes.
///
/// A decoder is allowed to answer a request with fewer rows than were asked for, and one in this
/// tree does: the `passthrough` example caps a scan at a thousand and twenty four rows whatever the
/// request said. That is legal, so a caller that wants a range asks again from where the last answer
/// stopped. It is what makes a partition boundary the host's rather than the decoder's.
///
/// A scan that produces no rows ends it. That is how a decoder says there is no more, and it is also
/// the only thing between a container claiming more rows than its data section holds and a loop that
/// never finishes.
fn fill(
    count: u64,
    mut scan: impl FnMut(u64, u64) -> Result<Vec<RecordBatch>, Error>,
) -> Result<Vec<RecordBatch>, Error> {
    let mut batches = Vec::new();
    let mut read = 0;
    while read < count {
        let more = scan(read, count - read)?;
        let rows: u64 = more
            .iter()
            .map(|batch| u64::try_from(batch.num_rows()).unwrap_or(u64::MAX))
            .sum();
        if rows == 0 {
            break;
        }
        batches.extend(more);
        read += rows;
    }
    Ok(batches)
}

/// What every partition of every query on this table has asked of the source so far.
///
/// Two counters and not a lock, because the partitions of one query run at the same time and the
/// thing being counted is a sum. Relaxed ordering, because nothing here is used to decide whether
/// something else happened: a reader wants the total, and the total is correct under relaxed
/// ordering because addition is.
#[derive(Debug, Default)]
pub(crate) struct Counter {
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl Counter {
    /// Adds what one partition cost.
    pub(crate) fn add(&self, traffic: Traffic) {
        self.requests.fetch_add(traffic.requests, Ordering::Relaxed);
        self.bytes.fetch_add(traffic.bytes, Ordering::Relaxed);
    }

    /// The running total.
    pub(crate) fn read(&self) -> Traffic {
        Traffic {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}
