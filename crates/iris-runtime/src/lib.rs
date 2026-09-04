//! The iris runtime an engine embeds.
//!
//! This is where a container stops being bytes and starts being Arrow. It opens a file, checks that
//! the decoder in it is the decoder the file names, compiles it, negotiates with it, asks it for
//! rows, and assembles what comes back into `RecordBatch`es an engine can consume.
//!
//! ```no_run
//! use iris_runtime::Runtime;
//!
//! let bytes = std::fs::read("readings.iris")?;
//! let runtime = Runtime::new()?;
//! let dataset = runtime.open(&bytes)?;
//!
//! println!("{} rows of {}", dataset.rows(), dataset.schema());
//! for batch in dataset.scan()? {
//!     println!("{} rows", batch.num_rows());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it checks
//!
//! Three things, in this order, because the order is the point.
//!
//! The decoder module is hashed and compared against the container before it is compiled. Compiling
//! is the first step that treats those bytes as code, so the check has to come before it rather
//! than alongside it.
//!
//! The ABI the container declares is checked before the module is compiled too, and a refusal names
//! the version, the decoder digest and the schema. Somebody holding a dataset they cannot open needs
//! to know which host would read it, which decoder to go and find, and whether this is even the
//! dataset they were after, and none of that is in scope by the time a refusal comes back out of a
//! guest.
//!
//! The host and the decoder negotiate, and both sides check. A decoder that agrees to terms it
//! cannot meet and a host that runs a decoder it cannot serve are different bugs, and finding out
//! which one happened is worth the second check.
//!
//! Every batch is counted against the schema. The schema says how many arrays there are and how
//! many buffers each one takes, and a batch that hands over a different number runs out or has some
//! left over. Then Arrow validates the buffers themselves. A decoder cannot produce an array that
//! is merely plausible.
//!
//! # What it does not do yet
//!
//! The whole source is copied into the guest at once, so a dataset has to fit in a wasm32 address
//! space and pays a copy on the way in. That is M1 being about the contract rather than the
//! throughput, and M4 is where the window arrives. No decoder changes when it does, because a
//! decoder only ever sees the range calls the SDK makes on its behalf.
//!
//! Nothing is metered. A decoder that loops forever loops forever, which is survivable while every
//! decoder in the tree is one somebody in this repository wrote and stops being survivable the
//! moment one is not. That is M2 and it is a gate rather than a nice to have.
//!
//! Unions, dictionaries, run end encoding and the view types are refused by name rather than
//! skipped. A column that quietly does not arrive is the worst failure this crate could have.

#![forbid(unsafe_code)]

mod assemble;
mod dataset;
mod error;
mod schema;

pub use dataset::{Dataset, Runtime};
pub use error::{Error, Result};
pub use schema::{schema_from_ipc, schema_to_ipc};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
