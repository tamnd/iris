//! Writes the same rows three ways, so the cost of carrying a container can be measured.
//!
//! ```text
//! cargo run --release -p iris-parquet --example variants -- sample.iris target/parquet
//! ```
//!
//! The three files hold the same rows, written by the same writer with the same properties, and
//! differ in one thing each.
//!
//! `plain.parquet` carries no container. It is the control, and it is what the file would have been
//! if nobody had ever thought of any of this.
//!
//! `gap.parquet` is what `iris-parquet` writes: the container between the last row group and the
//! footer, with two short keys in the metadata pointing at it.
//!
//! `inline.parquet` is the obvious alternative, which is the container base64 encoded into the file
//! metadata, where Parquet says extensions belong. It reads the same and it costs something
//! different, and `ci/parquet-cost.py` is what says how much.
//!
//! Writing that third one is the only reason there is a base64 encoder in this tree. It is here in
//! an example rather than in the library because the library does not do this and is not going to.

use std::env;
use std::fs;
use std::process::ExitCode;
use std::sync::Arc;

use iris_runtime::Runtime;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;

/// The key the alternative would have used, alongside the two real ones.
const INLINE_KEY: &str = "iris.container.inline";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [container, directory] = args.as_slice() else {
        eprintln!("usage: variants <container> <output directory>");
        return ExitCode::FAILURE;
    };

    match run(container, directory) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(container: &str, directory: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(container)?;
    let runtime = Runtime::new()?;
    let dataset = runtime.open(&bytes)?;
    let schema = dataset.schema().clone();
    let batches = dataset.scan()?;
    let rows: usize = batches.iter().map(arrow_array::RecordBatch::num_rows).sum();

    fs::create_dir_all(directory)?;
    let plain = format!("{directory}/plain.parquet");
    let gap = format!("{directory}/gap.parquet");
    let inline = format!("{directory}/inline.parquet");

    let mut writer = ArrowWriter::try_new(fs::File::create(&plain)?, Arc::clone(&schema), None)?;
    for batch in &batches {
        writer.write(batch)?;
    }
    writer.close()?;

    iris_parquet::write(fs::File::create(&gap)?, &schema, &batches, &bytes)?;

    let encoded = base64(&bytes);
    let mut writer = ArrowWriter::try_new(fs::File::create(&inline)?, Arc::clone(&schema), None)?;
    for batch in &batches {
        writer.write(batch)?;
    }
    writer.append_key_value_metadata(KeyValue::new(INLINE_KEY.to_owned(), encoded));
    writer.close()?;

    println!("rows={rows}");
    println!("container={}", bytes.len());
    println!("plain={}", fs::metadata(&plain)?.len());
    println!("gap={}", fs::metadata(&gap)?.len());
    println!("inline={}", fs::metadata(&inline)?.len());
    Ok(())
}

/// The container base64 encoded, standard alphabet, padded.
///
/// A Parquet metadata value is a string and a container is not, so anything that puts one in there
/// has to encode it. `ci/parquet-cost.py` decodes this with Python's own base64 and checks the
/// result against the container the other file carries whole, which is what keeps a hand written
/// encoder in an example honest.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let bits =
            (usize::from(group[0]) << 16) | (usize::from(group[1]) << 8) | usize::from(group[2]);

        let sextets = [
            (bits >> 18) & 0x3f,
            (bits >> 12) & 0x3f,
            (bits >> 6) & 0x3f,
            bits & 0x3f,
        ];
        for (index, sextet) in sextets.into_iter().enumerate() {
            out.push(if index <= chunk.len() {
                ALPHABET[sextet]
            } else {
                b'='
            });
        }
    }

    String::from_utf8(out).expect("the alphabet and the padding are both ascii")
}
