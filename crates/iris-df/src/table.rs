//! The table an engine registers, and how a query on it becomes a plan.

use std::path::Path;
use std::sync::Arc;

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use datafusion::catalog::{Session, TableProvider};
use datafusion::error::Result as DataFusionResult;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown, TableType};
use datafusion::physical_plan::ExecutionPlan;
use iris_runtime::{Capability, Digest, Runtime, Traffic};

use crate::error::Error;
use crate::exec::{IrisExec, Pushdown};
use crate::open::{Counter, Open, Resident, Stored};

/// The fewest rows worth giving a partition of its own.
///
/// Splitting is not free here the way it is for a file format the engine reads itself. Each
/// partition opens the container, and opening compiles the decoder, so a scan of a few thousand rows
/// cut eight ways is eight module compilations to save a few milliseconds of decoding. The number is
/// the default batch size, which means a partition is at least one batch and a small table is read
/// by one worker.
const ROWS_PER_PARTITION: u64 = 8_192;

/// An iris container, registered as a table.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use datafusion::prelude::SessionContext;
/// use iris_df::IrisTable;
/// use iris_runtime::Runtime;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let runtime = Runtime::new()?;
/// let table = IrisTable::open(&runtime, "readings.iris".as_ref())?;
///
/// let ctx = SessionContext::new();
/// ctx.register_table("readings", Arc::new(table))?;
///
/// let rows = ctx.sql("select c1 from readings").await?.collect().await?;
/// println!("{} batches", rows.len());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct IrisTable {
    open: Arc<dyn Open>,
    schema: SchemaRef,
    rows: u64,
    name: String,
    digest: Digest,
    pushes_projection: bool,
    counter: Arc<Counter>,
}

impl IrisTable {
    /// A table over a container in a file, read through a window.
    ///
    /// Nothing is read here except the header, the footer and the decoder module, which together
    /// come to about a kilobyte whatever the file is. The rows are read by the query.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Source`] if the file cannot be opened or the metadata cannot be read, and
    /// [`Error::Dataset`] if the container is not one this host will run.
    pub fn open(runtime: &Runtime, path: &Path) -> Result<Self, Error> {
        Self::over(Arc::new(Stored::new(runtime.clone(), path.to_path_buf())))
    }

    /// A table over a container that is already in memory.
    ///
    /// The buffer is shared rather than copied, so registering the same bytes as two tables costs
    /// two handles. What this path gives up is the traffic count: the bytes were paid for before
    /// the table existed, so [`IrisTable::traffic`] stays at zero however much of them a query
    /// reads, and a projection here is the decoder doing less work rather than the host moving
    /// fewer bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Dataset`] if the container is not one this host will run.
    pub fn resident(runtime: &Runtime, bytes: Arc<[u8]>) -> Result<Self, Error> {
        Self::over(Arc::new(Resident::new(runtime.clone(), bytes)))
    }

    /// Opens the container once, to learn everything a planner will ask about.
    fn over(open: Arc<dyn Open>) -> Result<Self, Error> {
        let meta = open.meta()?;
        Ok(Self {
            open,
            schema: meta.schema,
            rows: meta.rows,
            name: meta.name,
            digest: meta.digest,
            // Asked once here rather than once per scan. The answer is a property of the decoder
            // and the terms this host offers, and neither of those changes between two queries on
            // one table.
            pushes_projection: meta.agreed.contains(Capability::PROJECTION),
            counter: Arc::new(Counter::default()),
        })
    }

    /// How many rows the container says it has.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// What the container calls itself, which is not what it was registered as.
    #[must_use]
    pub fn dataset_name(&self) -> &str {
        &self.name
    }

    /// The identity of the decoder that will run, which is the hash of its bytes.
    #[must_use]
    pub const fn decoder_digest(&self) -> Digest {
        self.digest
    }

    /// Whether a projection reaches the decoder or is applied to the batches it produced.
    ///
    /// A decoder that never agreed to [`Capability::PROJECTION`] reads every column whatever it is
    /// asked for, so this table reads every column and cuts the ones the query wanted out of each
    /// batch. The answer is the same and the bytes moved are not, which is the whole reason this is
    /// worth being able to ask.
    #[must_use]
    pub const fn pushes_projection(&self) -> bool {
        self.pushes_projection
    }

    /// What every query on this table has asked of the source so far.
    ///
    /// This is where a pushdown becomes visible. A scan of three columns out of forty over a file
    /// moves about three fortieths of the data section, and if it moves all of it then the
    /// projection was applied after the fact. Opening the container is not counted, because it costs
    /// the same reads whatever the query was.
    ///
    /// Always zero for a table built with [`IrisTable::resident`], which is the honest answer for
    /// bytes somebody else already paid for.
    #[must_use]
    pub fn traffic(&self) -> Traffic {
        self.counter.read()
    }

    /// The schema a scan will produce, and how the columns get cut out.
    fn pushdown(&self, projection: Option<&Vec<usize>>) -> Result<(SchemaRef, Pushdown), Error> {
        let Some(indices) = projection else {
            return Ok((Arc::clone(&self.schema), Pushdown::Everything));
        };

        // Bounds checked here rather than left to Arrow, because the message is the value of it. A
        // planner does not produce an index out of range, so one that arrives means a host built a
        // projection by hand, and a sentence saying which index and how many there are ends that in
        // one read.
        let columns = self.schema.fields().len();
        let mut named = Vec::with_capacity(indices.len());
        for &at in indices {
            let column =
                u32::try_from(at)
                    .ok()
                    .filter(|_| at < columns)
                    .ok_or(Error::Projection {
                        column: at,
                        columns,
                    })?;
            named.push(column);
        }

        let schema = Arc::new(self.schema.project(indices)?);
        let pushdown = if self.pushes_projection {
            Pushdown::Decoder(named.into())
        } else {
            Pushdown::Host(indices.as_slice().into())
        };
        Ok((schema, pushdown))
    }
}

#[async_trait]
impl TableProvider for IrisTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> DataFusionResult<Arc<dyn ExecutionPlan>> {
        let (schema, pushdown) = self.pushdown(projection)?;

        // A limit is a prefix of the table, so it is honoured by reading fewer rows rather than by
        // reading them all and throwing some away. The engine still puts a limit above this, which
        // is what makes it safe to spread the prefix over several partitions: what this produces is
        // exactly the first `limit` rows, in some order, and the node above takes `limit` of them.
        let rows = match limit {
            Some(limit) => self.rows.min(u64::try_from(limit).unwrap_or(u64::MAX)),
            None => self.rows,
        };

        Ok(Arc::new(IrisExec::new(
            Arc::clone(&self.open),
            schema,
            pushdown,
            split(rows, state.config().target_partitions()).into(),
            Arc::clone(&self.counter),
        )))
    }

    /// Nothing, and this is written out rather than inherited so that the reason is somewhere.
    ///
    /// The ABI has a place for this. A `ScanRequest` carries a `filter` field and
    /// `Capability::FILTER_PUSHDOWN` is a bit a decoder can agree to. What does not exist yet is an
    /// agreed encoding for what goes in that field, and no decoder in the tree implements the bit,
    /// so a filter sent through it today would be bytes nobody reads.
    ///
    /// Saying `Inexact` here would mean claiming that the scan does some of the filtering, which
    /// would leave the engine free to keep the filter above the scan and also to believe the scan
    /// helped. It would not have. Saying `Unsupported` costs nothing but the filter running where
    /// it runs now, and it stops being a lie the day the encoding is agreed.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DataFusionResult<Vec<TableProviderFilterPushDown>> {
        Ok(vec![
            TableProviderFilterPushDown::Unsupported;
            filters.len()
        ])
    }
}

/// Cuts `rows` into at most `wanted` contiguous pieces, as evenly as they divide.
///
/// Always at least one piece, including for an empty table, because a plan with no partitions is a
/// plan an engine cannot execute. The remainder goes to the first pieces one row at a time rather
/// than all to the last, so the largest partition is one row bigger than the smallest whatever the
/// numbers are.
fn split(rows: u64, wanted: usize) -> Vec<(u64, u64)> {
    let wanted = u64::try_from(wanted).unwrap_or(u64::MAX).max(1);
    let parts = wanted.min(rows.div_ceil(ROWS_PER_PARTITION)).max(1);

    let each = rows / parts;
    let over = rows % parts;
    let mut out = Vec::with_capacity(usize::try_from(parts).unwrap_or(1));
    let mut at = 0;
    for part in 0..parts {
        let count = each + u64::from(part < over);
        out.push((at, count));
        at += count;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ROWS_PER_PARTITION, split};

    #[test]
    fn an_empty_table_is_one_empty_partition() {
        assert_eq!(split(0, 8), vec![(0, 0)]);
    }

    #[test]
    fn a_small_table_is_read_by_one_worker() {
        assert_eq!(split(100, 8), vec![(0, 100)]);
        assert_eq!(split(ROWS_PER_PARTITION, 8), vec![(0, ROWS_PER_PARTITION)]);
    }

    #[test]
    fn the_pieces_cover_the_rows_once_each() {
        for rows in [1, 7, ROWS_PER_PARTITION * 3, ROWS_PER_PARTITION * 7 + 1] {
            for wanted in 1..=9 {
                let parts = split(rows, wanted);
                assert!(!parts.is_empty(), "a plan needs at least one partition");

                let mut at = 0;
                for &(start, count) in &parts {
                    assert_eq!(start, at, "the pieces are contiguous and in order");
                    at += count;
                }
                assert_eq!(at, rows, "the pieces cover every row and no more");

                let widest = parts.iter().map(|&(_, count)| count).max().unwrap_or(0);
                let narrowest = parts.iter().map(|&(_, count)| count).min().unwrap_or(0);
                assert!(
                    widest - narrowest <= 1,
                    "the remainder is spread one row at a time"
                );
            }
        }
    }
}
