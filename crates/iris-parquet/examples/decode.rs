//! Reads a Parquet file through the iris container inside it, rather than as Parquet.
//!
//! ```text
//! cargo run --release -p iris-parquet --example decode -- sample.parquet
//! ```
//!
//! This is the other direction. Nothing here opens a data page or asks the Parquet reader for a
//! row. It reads the footer, finds out from two keys that this file carries a container, reads that
//! range and nothing else, and hands it to the host, which runs the decoder the container carries.
//! The numbers it prints are the ones `ci/parquet-embed.sh` compares against what pyarrow and
//! DuckDB say about the same file, and the whole claim is that all three agree.
//!
//! The decoder hint is printed before the container is opened, because that is the point of it. It
//! comes out of the Parquet footer, so a host can decide whether it wants anything to do with this
//! file before it reads another byte. What it says is not evidence, and `iris-parquet`'s
//! documentation says why.

use std::env;
use std::fs::File;
use std::process::ExitCode;

use arrow_array::{Int64Array, RecordBatch};
use iris_runtime::Runtime;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [path] = args.as_slice() else {
        eprintln!("usage: decode <parquet file>");
        return ExitCode::FAILURE;
    };

    match run(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let metadata = parquet::file::metadata::ParquetMetaDataReader::new().parse_and_finish(&file)?;

    match iris_parquet::decoder_hint(&metadata)? {
        Some(hint) => println!(
            "decoder={} abi={}.{} digest={}",
            hint.name,
            hint.abi_major,
            hint.abi_minor,
            hint.digest.short()
        ),
        None => println!("decoder=none"),
    }

    let Some(container) = iris_parquet::container(&file)? else {
        return Err(format!("{path} does not carry an iris container").into());
    };

    let runtime = Runtime::new()?;
    let dataset = runtime.open(&container)?;
    let batches = dataset.scan()?;

    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let mut sum = 0i64;
    for batch in &batches {
        for index in 0..batch.num_columns() {
            let column = batch
                .column(index)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or("this example only adds up integer columns")?;
            for row in 0..column.len() {
                sum = sum.wrapping_add(column.value(row));
            }
        }
    }

    println!("name={}", dataset.name());
    println!("columns={}", dataset.schema().fields().len());
    println!("rows={rows}");
    println!("sum={sum}");
    Ok(())
}
