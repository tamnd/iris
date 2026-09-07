//! A thousand queries over a pool that moves work between threads.
//!
//! The gate this holds is the second M6 one in `docs/ROADMAP.md`. `iris-vm/tests/stealing.rs` holds
//! the first half of it from underneath, by resuming a guest stack on a thread that has never seen
//! it before, once per suspension. This is the same property from above: the executor that actually
//! moves the work, running the code a host would run, on the query engine the prior art's assertion
//! would have been unsound under.
//!
//! The prior art declares `unsafe impl Send` on its job type and then checks at run time that the
//! thread it is on is the thread it started on. That is correct under a harness that pins work to
//! threads and it is unsound under any executor that moves a task after it parks. Tokio moves tasks
//! after they park and `DataFusion` runs on Tokio, so this test is the one that would have caught it.
//!
//! # What makes it a stress run rather than a loop
//!
//! Queries are in flight together rather than one after another. A query on its own gives a work
//! stealing pool nothing to steal, because a worker with an empty queue and no other queue to raid
//! parks instead. So each wave puts [`IN_FLIGHT`] queries on a runtime with four workers, each query
//! is planned across several partitions, and every partition ends in a `spawn_blocking` call that
//! returns on whichever thread the blocking pool used. That is the shape a stolen task arrives in.
//!
//! # What it checks
//!
//! A count and a sum rather than the rows. Both are aggregates the engine computes above the scan,
//! across partitions, so a row that went missing, a row that arrived twice and a batch assembled
//! from the wrong offset each change one of the two numbers. Comparing twenty thousand rows a
//! thousand times over would spend the whole test sorting integers, and it would not catch anything
//! these two do not.

mod support;

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use datafusion::catalog::TableProvider;
use datafusion::prelude::{SessionConfig, SessionContext};
use iris_df::IrisTable;
use iris_runtime::Runtime;

use support::{container, write};

/// Enough rows that a scan is split, which is what puts more than one thing on the pool per query.
const ROWS: u64 = 20_000;

/// Three columns, so a projected query and a whole one are different amounts of work.
const COLUMNS: u64 = 3;

/// How many queries the run makes in total.
const ITERATIONS: usize = 1_000;

/// How many of them are in flight at once.
///
/// Four times the worker count. Fewer than the workers and there is nothing to steal, and far more
/// than this only lengthens the queue: what is being exercised is a task that parks on one thread
/// and is picked up by another, and a backlog does not make that more likely per query.
const IN_FLIGHT: usize = 16;

/// The count and the sum a query came back with.
fn answer(batches: &[RecordBatch]) -> (i64, i64) {
    let [batch] = batches else {
        panic!(
            "an aggregate over one table is one batch, and this was {}",
            batches.len()
        )
    };
    let scalar = |at: usize| {
        batch
            .column(at)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("count and sum of an i64 column are both i64")
            .value(0)
    };
    (scalar(0), scalar(1))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_queries_across_a_stealing_pool_all_agree() {
    let runtime = Runtime::new().expect("a runtime starts");
    let scratch = write("stealing", &container(ROWS, COLUMNS));
    let table = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));

    let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(4));
    ctx.register_table("readings", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");

    // Column zero holds the row index, so the sum of it is the sum of the integers below `ROWS`.
    let rows = i64::try_from(ROWS).expect("the row count fits");
    let wanted = (rows, rows * (rows - 1) / 2);

    let mut done = 0;
    while done < ITERATIONS {
        let wave = IN_FLIGHT.min(ITERATIONS - done);
        let queries = (0..wave).map(|_| {
            let ctx = ctx.clone();
            async move {
                let batches = ctx
                    .sql("select count(c0), sum(c0) from readings")
                    .await
                    .expect("the query plans")
                    .collect()
                    .await
                    .expect("the query runs");
                answer(&batches)
            }
        });

        for (at, got) in futures::future::join_all(queries)
            .await
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                got,
                wanted,
                "query {} of the run came back with a different table than the one before it",
                done + at
            );
        }
        done += wave;
    }
}
