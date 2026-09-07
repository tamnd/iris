//! Queries against iris tables, and what those queries cost.
//!
//! Two things are checked here and they are not the same thing. One is that a select over an iris
//! table gives the answer the container holds, which is what the batches say. The other is that a
//! projection reached the decoder rather than being applied to rows that had already been read,
//! which the batches cannot say at all, because both arrangements produce identical output. The only
//! place the difference shows up is in what the source was asked for, which is why these tests read
//! `IrisTable::traffic` as often as they read values.
//!
//! Each table is held here as well as registered, so that the counter can be read after the query
//! without going back through the catalog to find it.

mod support;

use std::any::Any;
use std::sync::Arc;

use datafusion::catalog::TableProvider;
use datafusion::physical_plan::{ExecutionPlan, ExecutionPlanProperties, displayable};
use datafusion::prelude::{SessionConfig, SessionContext};
use iris_df::{IrisExec, IrisTable, Pushdown, Traffic};
use iris_runtime::Runtime;

use support::{cell, container, flat_container, values, write};

/// Wide enough that a projection is a real saving, and short enough to stay quick.
const ROWS: u64 = 20_000;

/// Three columns, so one of them is about a third of the data section.
const COLUMNS: u64 = 3;

/// Rows enough that the file does not fit in one window, which is what makes traffic mean anything.
///
/// A file source opens with `iris_source::DEFAULT_SPAN` of address space, four mebibytes, and a
/// range served out of the view it already holds costs nothing and is not counted. That is the point
/// of the counter and it is also a trap for a test: a fixture smaller than the window is read
/// entirely out of the first view, so a scan of one column and a scan of three both report zero and
/// the two look identical. Three columns of about two and a third mebibytes each is a file the
/// window has to be moved around, and one column of it still fits in a view.
const WIDE_ROWS: u64 = 300_000;

/// A context that will spread a scan over this many workers and no more.
fn context(partitions: usize) -> SessionContext {
    SessionContext::new_with_config(SessionConfig::new().with_target_partitions(partitions))
}

/// The scan node inside a plan, which here is the plan, because a scan is a leaf.
///
/// Reached through `Any` rather than through a method on the trait, because `ExecutionPlan` has no
/// `as_any` of its own in `DataFusion` 55 and does not need one: the trait requires `Any`, so an
/// upcast is all it takes.
fn scan_node(plan: &Arc<dyn ExecutionPlan>) -> &IrisExec {
    (plan.as_ref() as &dyn Any)
        .downcast_ref::<IrisExec>()
        .expect("an iris table produces an IrisExec")
}

/// Every value a query produced, in one column of its output.
async fn run(ctx: &SessionContext, sql: &str) -> Vec<i64> {
    let batches = ctx
        .sql(sql)
        .await
        .expect("the query plans")
        .collect()
        .await
        .expect("the query runs");
    values(&batches, 0)
}

#[tokio::test]
async fn a_query_over_a_resident_table_returns_the_rows() {
    let runtime = Runtime::new().expect("a runtime starts");
    let table = Arc::new(
        IrisTable::resident(&runtime, container(ROWS, COLUMNS).into())
            .expect("the container opens"),
    );

    assert_eq!(table.rows(), ROWS);
    assert_eq!(table.dataset_name(), "readings");
    assert!(table.pushes_projection(), "fixedwidth agreed to projection");

    let ctx = context(1);
    ctx.register_table("readings", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");
    let batches = ctx
        .sql("select c0, c1, c2 from readings")
        .await
        .expect("the query plans")
        .collect()
        .await
        .expect("the query runs");

    for column in 0..COLUMNS {
        let wanted: Vec<i64> = (0..ROWS).map(|row| cell(column, row)).collect();
        let at = usize::try_from(column).expect("three columns fit in a usize");
        assert_eq!(
            values(&batches, at),
            wanted,
            "column {column} came back holding other bytes"
        );
    }

    // Nothing was fetched, because nothing had to be. The buffer was paid for before the table
    // existed and reading a hundredth of it costs the same as reading all of it.
    assert_eq!(table.traffic(), Traffic::NONE);
}

#[tokio::test]
async fn a_projection_is_visible_in_what_the_source_was_asked_for() {
    let runtime = Runtime::new().expect("a runtime starts");
    let scratch = write("projection", &container(WIDE_ROWS, COLUMNS));

    // Two tables over one file rather than two queries on one table, because the counter is
    // cumulative and what is being compared is one query against another.
    let whole = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));
    let part = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));
    assert_eq!(whole.traffic(), Traffic::NONE, "opening is not a scan");

    let ctx = context(1);
    ctx.register_table("whole", Arc::clone(&whole) as Arc<dyn TableProvider>)
        .expect("the name is free");
    ctx.register_table("part", Arc::clone(&part) as Arc<dyn TableProvider>)
        .expect("the name is free");

    let every = run(&ctx, "select c0, c1, c2 from whole").await;
    assert_eq!(every.len(), usize::try_from(WIDE_ROWS).expect("it fits"));
    let all = whole.traffic();

    let one = run(&ctx, "select c1 from part").await;
    let wanted: Vec<i64> = (0..WIDE_ROWS).map(|row| cell(1, row)).collect();
    assert_eq!(one, wanted, "the projected column is the wrong one");
    let projected = part.traffic();

    // A third of the data, with room either side. The decoder reads in blocks and the blocks do not
    // line up with a column boundary, so the exact number is the decoder's business. What is
    // asserted is that a fraction of the file was read rather than all of it, which is the whole
    // difference between a projection the decoder acted on and one this crate applied to rows that
    // had already been moved.
    assert!(projected.bytes > 0, "reading a column moves bytes");
    assert!(
        projected.bytes * 2 < all.bytes,
        "one column of three moved {} bytes and all three moved {}, so the projection did not \
         reach the decoder",
        projected.bytes,
        all.bytes
    );
    assert!(
        projected.requests < all.requests,
        "one column of three took {} requests and all three took {}",
        projected.requests,
        all.requests
    );
}

#[tokio::test]
async fn a_decoder_that_cannot_project_still_gives_the_right_answer() {
    let runtime = Runtime::new().expect("a runtime starts");
    let scratch = write("flat", &flat_container(ROWS));
    let table = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));

    assert!(
        !table.pushes_projection(),
        "passthrough never agreed to projection"
    );

    // Planned through the provider rather than through SQL, so where the projection lands is read
    // off the plan rather than guessed at from the numbers.
    let ctx = context(1);
    let plan = table
        .scan(&ctx.state(), Some(&vec![0]), &[], None)
        .await
        .expect("the scan plans");
    assert_eq!(
        scan_node(&plan).pushdown(),
        &Pushdown::Host(vec![0usize].into()),
        "a decoder without projection is not told which columns to read"
    );

    ctx.register_table("flat", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");

    // The passthrough decoder caps a scan at a thousand and twenty four rows whatever it was asked
    // for, so a partition covering more than that takes several scans. Every row turning up exactly
    // once is what says the asking again picked up where the last answer stopped.
    let wanted: Vec<i64> = (0..ROWS).map(|row| cell(0, row)).collect();
    assert_eq!(run(&ctx, "select c0 from flat").await, wanted);
}

#[tokio::test]
async fn a_filter_runs_above_the_scan_and_is_still_right() {
    let runtime = Runtime::new().expect("a runtime starts");
    let table = Arc::new(
        IrisTable::resident(&runtime, container(ROWS, COLUMNS).into())
            .expect("the container opens"),
    );

    let ctx = context(1);
    ctx.register_table("readings", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");

    let sql = "select c0 from readings where c0 < 10";
    let wanted: Vec<i64> = (0..10).map(|row| cell(0, row)).collect();
    assert_eq!(run(&ctx, sql).await, wanted);

    // Asserted here as well as documented in the provider, because the day a filter encoding is
    // agreed this is the test that should fail and be rewritten rather than quietly keep passing.
    let physical = ctx
        .sql(sql)
        .await
        .expect("the query plans")
        .create_physical_plan()
        .await
        .expect("it turns into a plan");
    let plan = format!("{}", displayable(physical.as_ref()).indent(false));
    assert!(
        plan.contains("FilterExec"),
        "nothing is pushed down, so the filter should be a node above the scan:\n{plan}"
    );
}

#[tokio::test]
async fn a_scan_is_split_across_the_workers_it_was_given() {
    let runtime = Runtime::new().expect("a runtime starts");
    let scratch = write("partitions", &container(ROWS, COLUMNS));
    let table = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));

    let ctx = context(4);
    let plan = table
        .scan(&ctx.state(), None, &[], None)
        .await
        .expect("the scan plans");
    assert_eq!(
        plan.output_partitioning().partition_count(),
        3,
        "twenty thousand rows at eight thousand one hundred and ninety two to a partition is \
         three, and four workers were on offer"
    );

    ctx.register_table("readings", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");

    let wanted: Vec<i64> = (0..ROWS).map(|row| cell(2, row)).collect();
    assert_eq!(
        run(&ctx, "select c2 from readings").await,
        wanted,
        "the partitions overlap or leave a gap"
    );
}

#[tokio::test]
async fn a_limit_reads_a_prefix_rather_than_the_table() {
    let runtime = Runtime::new().expect("a runtime starts");
    let scratch = write("limit", &container(ROWS, COLUMNS));
    let table = Arc::new(IrisTable::open(&runtime, &scratch.0).expect("the container opens"));

    let ctx = context(1);
    let plan = table
        .scan(&ctx.state(), None, &[], Some(100))
        .await
        .expect("the scan plans");
    assert_eq!(
        scan_node(&plan).parts().to_vec(),
        vec![(0, 100)],
        "a limit is honoured by reading fewer rows rather than by throwing rows away"
    );

    ctx.register_table("readings", Arc::clone(&table) as Arc<dyn TableProvider>)
        .expect("the name is free");

    let wanted: Vec<i64> = (0..100).map(|row| cell(0, row)).collect();
    assert_eq!(run(&ctx, "select c0 from readings limit 100").await, wanted);
}
