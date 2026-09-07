//! Why a table could not be opened, or a scan could not be run.

use datafusion::error::DataFusionError;

/// What went wrong between a query and a container.
///
/// Four variants, and three of them are somebody else's error kept whole. This crate is a
/// translator rather than a reader: the reading is done by `iris-runtime`, the bytes are moved by
/// `iris-source` and the batches are built by Arrow, so an error from any of them should arrive at
/// the engine saying what it said rather than saying that iris-df could not read a table.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The runtime refused the container, the decoder, or a batch the decoder produced.
    #[error("{0}")]
    Dataset(#[from] iris_runtime::Error),

    /// The bytes could not be reached.
    #[error("{0}")]
    Source(#[from] iris_source::SourceError),

    /// Arrow refused to build something this crate asked it for.
    ///
    /// A projected schema, or a batch cut down to the columns a query wanted. Both are Arrow doing
    /// work on behalf of a projection this crate has already bounds checked, so one of these means
    /// something further apart than an index out of range.
    #[error("{0}")]
    Arrow(#[from] arrow_schema::ArrowError),

    /// A query asked for a column the table does not have.
    ///
    /// A planner does not produce one of these, because it projects against the schema this table
    /// handed it. It is here because the projection is a list of indices by the time it arrives and
    /// an index is checkable, so it gets checked rather than being trusted into a decoder.
    #[error("column {column} was asked for and this table has {columns}")]
    Projection {
        /// The index that was asked for.
        column: usize,
        /// How many columns the table has.
        columns: usize,
    },
}

// Every error this crate produces reaches DataFusion through this, and it goes in as `External`
// rather than as a message. A host that wants to know whether a query failed because a decoder was
// tampered with or because a network read timed out can downcast and ask, and flattening the cause
// into a string here would be the one step that makes that impossible.
impl From<Error> for DataFusionError {
    fn from(error: Error) -> Self {
        Self::External(Box::new(error))
    }
}
