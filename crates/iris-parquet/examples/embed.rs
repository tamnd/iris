//! Writes a Parquet file that carries an iris container.
//!
//! ```text
//! cargo run --release -p iris-parquet --example embed -- sample.iris sample.parquet
//! ```
//!
//! The rows go in twice, once as ordinary Parquet and once as the container they came out of. That
//! is the deal the whole idea rests on: the file is larger than either half, and in exchange a
//! reader that has never heard of iris reads it and a reader that has gets the decoder the writer
//! chose. Both numbers are printed, because a claim about a format that does not say what it costs
//! is not worth much.
//!
//! Decoding the container is the host's job rather than this crate's, so the runtime arrives here
//! as a dev-dependency and nothing that depends on `iris-parquet` pulls a wasm engine in behind it.

use std::env;
use std::fs;
use std::process::ExitCode;

use iris_runtime::Runtime;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [container, output] = args.as_slice() else {
        eprintln!("usage: embed <container> <parquet output>");
        return ExitCode::FAILURE;
    };

    match run(container, output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(container: &str, output: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(container)?;
    let runtime = Runtime::new()?;
    let dataset = runtime.open(&bytes)?;
    let schema = dataset.schema().clone();
    let batches = dataset.scan()?;

    let rows: usize = batches.iter().map(arrow_array::RecordBatch::num_rows).sum();
    let sink = fs::File::create(output)?;
    iris_parquet::write(sink, &schema, &batches, &bytes)?;

    let size = fs::metadata(output)?.len();
    println!("rows={rows}");
    println!("container={}", bytes.len());
    println!("parquet={size}");
    Ok(())
}
