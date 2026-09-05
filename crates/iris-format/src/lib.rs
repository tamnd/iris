//! The container that carries an iris dataset.
//!
//! A container is a header, a run of sections, a footer that describes them, and a trailer that
//! says where the footer is. The footer holds the schema, the reference to the decoder that reads
//! this dataset, and a digest for every section. Nothing in here knows anything about Arrow or
//! about `WebAssembly`; the schema and the decoder module are both carried as opaque bytes with a
//! digest, which is what keeps this crate small enough to read in one sitting and small enough to
//! fuzz seriously.
//!
//! The layout is written out in `docs/FORMAT.md` and in the module documentation for [`layout`].
//!
//! # Parsing is the untrusted path
//!
//! A dataset arrives from somewhere. Reading one must not panic, must not read out of bounds, and
//! must not allocate on the basis of a length field it has not checked. The crate forbids `unsafe`
//! outright, so out of bounds is a language guarantee rather than a promise. The other two are
//! properties of how the parser is written, and they are held up by tests and by a fuzz target.
//!
//! The allocation rule is the one that is easiest to get wrong, so the format is arranged to make
//! it hard: there is no count field anywhere in the footer. The number of sections is however many
//! section records the footer actually contains, so a file that claims a billion sections has to be
//! large enough to hold a billion section records.
//!
//! # Digests
//!
//! Each section carries the digest of its bytes, the footer carries the section records, and the
//! trailer carries a digest over the header and the footer. [`Container::parse`] checks the last of
//! those, because it is cheap and it makes a parsed container mean the metadata is what the writer
//! wrote. [`Container::verify`] checks the sections, which means reading the whole file, so it is a
//! separate decision that a caller makes once when a dataset arrives.

#![forbid(unsafe_code)]

mod build;
mod container;
mod digest;
mod directory;
mod error;
pub mod layout;
mod meta;

pub use build::Builder;
pub use container::{Container, FileHeader};
pub use digest::Digest;
pub use directory::{Directory, Placement};
pub use error::{Error, Result};
pub use layout::{DecoderLocation, FORMAT_MAJOR, FORMAT_MINOR, MAGIC, SchemaEncoding, SectionKind};
pub use meta::{Dataset, DecoderRef, Schema, Section};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
