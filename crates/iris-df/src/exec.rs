//! The physical plan node that reads rows out of a container.
//!
//! # One partition is one range of tuples
//!
//! A scan is split by row rather than by anything in the file. The plan holds a list of ranges
//! covering the rows the query asked for, one per output partition, and each partition opens the
//! container, reads its range, and is done. Nothing is exchanged between partitions and no partition
//! can see another one's window.
//!
//! That split is only safe because of what the M5 gate checks. A range read twice comes back byte
//! identical, and ranges read out of order are, one for one, what the same ranges read in sequence
//! produce. `iris-runtime/tests/harness.rs` checks both against every decoder in the tree, which is
//! what makes handing tuple ranges to a pool a plan rather than a hope.
//!
//! # The read is blocking, and says so
//!
//! Each partition does its work inside `tokio::task::spawn_blocking`. A scan of a windowed dataset
//! blocks the thread it is on whenever the decoder asks for a range that has not arrived, because
//! `iris_source::read_blocking` is what serves it, so running one on an executor thread would park
//! a worker that has other tasks waiting. Moving it to the blocking pool is the honest arrangement
//! for a read that blocks.
//!
//! It is also the arrangement iris #38 is about. When a miss yields the worker instead of holding
//! it, this becomes an ordinary task and the blocking pool goes away. Until then, a plan that
//! pretended otherwise would be quietly parking executor threads.

use std::fmt;
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::error::{DataFusionError, Result as DataFusionResult};
use datafusion::execution::TaskContext;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use futures::TryStreamExt as _;

use crate::error::Error;
use crate::open::{Counter, Open};

/// Where the columns a query asked for get cut out of the rows.
///
/// Three cases and no room for a fourth, which is why this is not marked as open to additions. The
/// query wanted everything, or it wanted some of it and the decoder can be told, or it wanted some
/// of it and the decoder cannot. The middle one is the case that moves fewer bytes and it is the
/// one a test asserting that a pushdown happened is looking for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pushdown {
    /// The query wanted every column, so there was nothing to push.
    Everything,

    /// The decoder agreed to projection and was told which columns to read.
    ///
    /// This is the case that reaches storage. The host does not decide which bytes to fetch, the
    /// decoder does, so a projection that shows up in the request count is one the decoder acted on.
    Decoder(Arc<[u32]>),

    /// The decoder does not do projection, so every column was read and these were taken afterwards.
    ///
    /// The answer is the same and the bytes moved are not. A decoder that never agreed to
    /// `Capability::PROJECTION` reads every column whatever it is asked for, and asking it anyway
    /// would be refused rather than served, so the projection happens here instead.
    Host(Arc<[usize]>),
}

impl Pushdown {
    /// What the decoder is told to read, which is nothing at all in the two other cases.
    ///
    /// Empty means every column, in the ABI and in `iris-runtime` both, so the two cases that do not
    /// push anything down are the same request to a decoder.
    #[must_use]
    pub fn told(&self) -> &[u32] {
        match self {
            Self::Decoder(columns) => columns,
            Self::Everything | Self::Host(_) => &[],
        }
    }

    /// The columns to take out of a batch once it has been read, if any.
    #[must_use]
    pub fn taken(&self) -> Option<&[usize]> {
        match self {
            Self::Host(columns) => Some(columns),
            Self::Everything | Self::Decoder(_) => None,
        }
    }
}

/// A scan of an iris container, split into one range of rows per partition.
///
/// Built by the table provider rather than by hand. It is public because a host writing a physical
/// optimizer rule, or a test asserting on a plan, needs to be able to name the node it found, and
/// because the two things worth asking a scan node are on it: which rows each partition reads, and
/// where the projection happened.
pub struct IrisExec {
    open: Arc<dyn Open>,
    schema: SchemaRef,
    pushdown: Pushdown,
    parts: Arc<[(u64, u64)]>,
    counter: Arc<Counter>,
    properties: Arc<PlanProperties>,
}

impl IrisExec {
    /// Builds a node reading `parts` out of `open`, producing batches of `schema`.
    pub(crate) fn new(
        open: Arc<dyn Open>,
        schema: SchemaRef,
        pushdown: Pushdown,
        parts: Arc<[(u64, u64)]>,
        counter: Arc<Counter>,
    ) -> Self {
        // No ordering is claimed. A container says nothing about how its rows are sorted, and the
        // rows of one partition arrive in order only within that partition, so claiming an ordering
        // would let the planner drop a sort that the data does not justify.
        let properties = PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&schema)),
            Partitioning::UnknownPartitioning(parts.len()),
            // Batches come out as the decoder produces them rather than all at the end.
            EmissionType::Incremental,
            Boundedness::Bounded,
        );

        Self {
            open,
            schema,
            pushdown,
            parts,
            counter,
            properties: Arc::new(properties),
        }
    }

    /// The rows each partition reads, as a start and a count.
    #[must_use]
    pub fn parts(&self) -> &[(u64, u64)] {
        &self.parts
    }

    /// Where the projection this scan was given gets applied.
    #[must_use]
    pub const fn pushdown(&self) -> &Pushdown {
        &self.pushdown
    }
}

impl fmt::Debug for IrisExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IrisExec")
            .field("parts", &self.parts)
            .field("pushdown", &self.pushdown)
            .finish_non_exhaustive()
    }
}

impl DisplayAs for IrisExec {
    fn fmt_as(&self, format: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: u64 = self.parts.iter().map(|&(_, count)| count).sum();
        match format {
            DisplayFormatType::Default | DisplayFormatType::Verbose => write!(
                f,
                "IrisExec: rows={rows}, partitions={}, projection={:?}",
                self.parts.len(),
                self.pushdown
            ),
            DisplayFormatType::TreeRender => write!(f, "rows={rows}"),
        }
    }
}

impl ExecutionPlan for IrisExec {
    fn name(&self) -> &'static str {
        "IrisExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    // A leaf that holds no expressions. The projection is a list of column indices rather than a
    // list of `PhysicalExpr`, and the filters were never accepted, so there is nothing here for an
    // optimizer rule rewriting expressions to find.
    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> DataFusionResult<TreeNodeRecursion>,
    ) -> DataFusionResult<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    // Deprecated in DataFusion 55 in favour of `replace_children`, and still the required method
    // that the default `replace_children` calls. A leaf node has no children to replace, so both
    // roads lead here and this hands back what it was given.
    #[allow(deprecated, reason = "the trait still requires this method")]
    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DataFusionResult<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> DataFusionResult<SendableRecordBatchStream> {
        let Some(&(start, count)) = self.parts.get(partition) else {
            return Err(DataFusionError::Internal(format!(
                "partition {partition} was asked for and this scan has {}",
                self.parts.len()
            )));
        };

        let schema = Arc::clone(&self.schema);
        let read = read_partition(
            Arc::clone(&self.open),
            self.pushdown.clone(),
            Arc::clone(&self.counter),
            start,
            count,
        );

        // One future producing one batch list, turned into a stream, rather than a channel and a
        // task behind it. The whole partition is one call into the decoder either way, so there is
        // nothing here for a second task to overlap with.
        let batches = futures::stream::once(read)
            .map_ok(|batches| futures::stream::iter(batches.into_iter().map(DataFusionResult::Ok)))
            .try_flatten();
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, batches)))
    }
}

/// Reads one partition's rows, on a thread that is allowed to block.
///
/// Written out rather than left inline so that the error type is named once. Everything in here
/// produces a different error and every one of them reaches the engine as the same type, which is
/// the sort of thing a reader should be able to see rather than infer.
async fn read_partition(
    open: Arc<dyn Open>,
    pushdown: Pushdown,
    counter: Arc<Counter>,
    start: u64,
    count: u64,
) -> DataFusionResult<Vec<RecordBatch>> {
    let told = pushdown.told().to_vec();
    let rows = tokio::task::spawn_blocking(move || open.rows(start, count, &told))
        .await
        .map_err(|joined| DataFusionError::External(Box::new(joined)))?
        .map_err(DataFusionError::from)?;
    counter.add(rows.traffic);

    match pushdown.taken() {
        None => Ok(rows.batches),
        Some(columns) => rows
            .batches
            .iter()
            .map(|batch| batch.project(columns))
            .collect::<Result<Vec<RecordBatch>, _>>()
            .map_err(|err| DataFusionError::from(Error::from(err))),
    }
}
