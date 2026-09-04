//! `iris`, the command line tool.
//!
//! Inspects, verifies and decodes iris datasets. Nothing is implemented yet;
//! see the milestone that owns this crate in `docs/ROADMAP.md`.

use clap::{Parser, Subcommand};

/// Command line interface for iris self-decoding datasets.
#[derive(Debug, Parser)]
#[command(name = "iris", version, about, long_about = None)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands `iris` understands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Print the metadata of a dataset without decoding it.
    Inspect {
        /// Path or URL of the dataset.
        target: String,
    },
    /// Check that a dataset matches the digests it carries.
    Verify {
        /// Path or URL of the dataset.
        target: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { target } | Command::Verify { target } => {
            anyhow::bail!("not implemented yet: {target}")
        }
    }
}
