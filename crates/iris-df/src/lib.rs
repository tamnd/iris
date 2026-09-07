//! A `DataFusion` table provider that reads iris datasets.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use datafusion::prelude::SessionContext;
//! use iris_df::IrisTable;
//! use iris_runtime::Runtime;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Runtime::new()?;
//! let table = IrisTable::open(&runtime, "readings.iris".as_ref())?;
//!
//! let ctx = SessionContext::new();
//! ctx.register_table("readings", Arc::new(table))?;
//!
//! let batches = ctx.sql("select c1 from readings").await?.collect().await?;
//! println!("{} batches", batches.len());
//! # Ok(())
//! # }
//! ```
//!
//! # What a scan turns into
//!
//! A query on an iris table becomes one plan node holding a list of tuple ranges, one per output
//! partition. Each partition opens the container, reads its own rows, and closes it. Nothing is
//! shared between them, and no partition can see another one's window.
//!
//! Splitting a scan by tuple range and handing the pieces to a pool is safe because of what the M5
//! gate checks rather than because it looks safe. A range read twice comes back byte identical, and
//! ranges read out of order are, one for one, what the same ranges read in sequence produce. Both
//! are checked against every decoder in the tree by `iris-runtime/tests/harness.rs`, on both the
//! resident path and the windowed one.
//!
//! # What gets pushed down
//!
//! **Projection, when the decoder agreed to it.** The columns a query asked for go into the
//! `ScanRequest`, so the decoder is the thing that decides which bytes to fetch and a projection
//! that reaches storage is one it acted on. [`IrisTable::traffic`] is where that shows up: a scan of
//! one column out of three over a file moves about a third of the data section, and if it moves all
//! of it then the projection was applied after the fact.
//!
//! A decoder that never agreed to `Capability::PROJECTION` reads every column whatever it is asked
//! for. Asking it anyway would be refused, so the table reads every column and cuts out the ones the
//! query wanted, which gives the same answer and moves the same bytes it would have moved anyway.
//! [`Pushdown`] is which of the two happened and [`IrisTable::pushes_projection`] says so before a
//! query runs.
//!
//! **Limit**, by reading a prefix of the table rather than by reading it all and throwing rows away.
//!
//! # What does not get pushed down
//!
//! **Filters.** The ABI has a place for one: a `ScanRequest` carries a `filter` field and
//! `Capability::FILTER_PUSHDOWN` is a bit a decoder can agree to. What does not exist yet is an
//! agreed encoding for what goes in that field, and no decoder in the tree implements the bit, so a
//! filter sent through it today would be bytes nobody reads. This provider reports every filter as
//! unsupported and the engine applies them above the scan, which is slower than it will be and is
//! not a claim about work that does not happen.
//!
//! # What it costs today
//!
//! Each partition opens the container, and opening compiles the decoder in it, so a scan split eight
//! ways compiles the module eight times. That is the honest cost of not holding a borrow of a buffer
//! the same struct owns, and it is what iris #127, the compiled module cache, is for. When a
//! `Runtime` remembers modules it has already compiled, the open becomes metadata and nothing else,
//! with no change here.
//!
//! Each partition also does its reading inside `tokio::task::spawn_blocking`, because a decoder that
//! misses on a range blocks the thread serving it. That is what iris #38 is about, and until a miss
//! yields the worker, a plan that ran this on an executor thread would be quietly parking one.

#![forbid(unsafe_code)]

mod error;
mod exec;
mod open;
mod table;

pub use error::Error;
pub use exec::{IrisExec, Pushdown};
pub use table::IrisTable;

/// What a scan asked of the source, in requests and bytes.
///
/// Re-exported because [`IrisTable::traffic`] hands one back. A host that wanted to check that a
/// projection reached storage should not have to add a dependency to name what it was given.
pub use iris_runtime::Traffic;

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
