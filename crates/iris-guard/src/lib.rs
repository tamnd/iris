//! Structural validation of Arrow arrays crossing the sandbox boundary.
//!
//! A sandbox stops a decoder from reading the host's memory. It does nothing at all about the
//! numbers the decoder hands back, and those numbers are offsets, lengths and buffer indices that
//! the host is about to use to read its own memory. That gap is the difference between a security
//! claim and a security property, and this crate is where the gap gets closed.
//!
//! ```
//! use arrow_schema::{DataType, Field, Schema};
//! use iris_abi::Node;
//!
//! let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
//! let nodes = [Node { length: 2, null_count: 0 }];
//! let buffers: Vec<Vec<u8>> = vec![Vec::new(), 1i64.to_le_bytes().into_iter().chain(2i64.to_le_bytes()).collect()];
//!
//! iris_guard::check(&schema, 2, &nodes, &buffers)?;
//! # Ok::<(), iris_guard::Violation>(())
//! ```
//!
//! # The property
//!
//! If [`check`] returns `Ok` then every offset in the batch is inside the buffer it indexes, every
//! buffer is long enough for the number of slots its array claims, and every child array is long
//! enough for the parent that points into it. Arrays that pass can be read without a read leaving
//! the bytes it was given.
//!
//! That is a bounds property and nothing more. Whether a `Utf8` column holds well formed UTF-8 is a
//! correctness question rather than a bounds question, because reading a badly encoded string
//! cannot leave the buffer, so it is left to Arrow. Keeping the line there is what keeps the surface
//! that gets fuzzed the surface whose failure is silent.
//!
//! # Why the checks are here and not left to Arrow
//!
//! Arrow validates an `ArrayData` when it is built, and that validation is good. It is also the
//! wrong place for two of these checks and the wrong shape for the rest.
//!
//! The wrong place, because a schema nested a hundred thousand deep and a length that overflows when
//! it is multiplied by a width both have to be refused *before* anything walks the schema or
//! allocates against the length. By the time there is an `ArrayData` to validate, the recursion has
//! already happened.
//!
//! The wrong shape, because what comes back is a message. This crate returns [`Invariant`], so a
//! host can count refusals by rule, alert on one kind and not another, and say which rule failed
//! without matching on prose.
//!
//! So the batch is checked here first, and then Arrow validates what it is handed as an independent
//! second opinion. That means the structural checks run twice, which is a real cost, measured rather
//! than assumed, and written down against a decision rule that was committed before the number was
//! known. Removing the second pass means building arrays with validation skipped, which is an
//! `unsafe` call in the runtime that this crate's fuzzer would have to be the only thing standing
//! behind. That trade is worth making when there is a number saying it matters and not before.
//!
//! # What it does not carry
//!
//! Unions, dictionaries, run end encoding and the view types are refused by name. Each needs
//! something the batch cannot carry yet, and the two whose checks are the hard part have those
//! checks written and tested anyway, in [`check_dictionary`] and [`check_views`].

#![forbid(unsafe_code)]

pub mod corpus;

mod check;
mod error;
mod indirect;
mod layout;

pub use check::{MAX_DEPTH, check, check_schema};
pub use error::{Invariant, Result, Violation};
pub use indirect::{VIEW_INLINE, VIEW_WIDTH, check_dictionary, check_views};
pub use layout::{Layout, layout};

/// The version of this crate, as reported by build metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
