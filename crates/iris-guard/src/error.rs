//! What the guard refuses, and the rule it refused under.

use core::fmt;

/// The rule an array broke.
///
/// This is an enum rather than a string because a host that wants to count refusals by kind, or
/// treat one kind differently from another, should not have to match on prose. The prose is in the
/// detail, which is written for whoever has to go and find the decoder that produced this.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum Invariant {
    /// The schema nests deeper than this crate will walk.
    Depth,
    /// The type is one this crate does not know how to check.
    Unsupported,
    /// The batch does not have the number of arrays the schema calls for.
    Arrays,
    /// The batch does not have the number of buffers the schema calls for.
    Buffers,
    /// A top level array is not as long as the batch says it is.
    Rows,
    /// A child array is shorter than its parent needs it to be.
    ChildLength,
    /// The declared null count is not what the validity bitmap says.
    NullCount,
    /// The validity bitmap has fewer bits than the array has slots.
    Validity,
    /// A buffer is shorter than the array's length requires.
    BufferLength,
    /// A length and a width multiply to more than this host can address.
    Size,
    /// Offsets run backwards.
    OffsetOrder,
    /// An offset points past the end of the thing it indexes.
    OffsetRange,
    /// A dictionary key is not a slot in the dictionary.
    DictionaryIndex,
    /// A view points at a data buffer that is not there, or past the end of one that is.
    ViewBuffer,
}

impl Invariant {
    /// The rule's name, as it appears in a message.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Depth => "nesting depth",
            Self::Unsupported => "known types",
            Self::Arrays => "array count",
            Self::Buffers => "buffer count",
            Self::Rows => "row count",
            Self::ChildLength => "child length",
            Self::NullCount => "null count",
            Self::Validity => "validity length",
            Self::BufferLength => "buffer length",
            Self::Size => "addressable size",
            Self::OffsetOrder => "offset order",
            Self::OffsetRange => "offset range",
            Self::DictionaryIndex => "dictionary index",
            Self::ViewBuffer => "view buffer",
        }
    }
}

impl fmt::Display for Invariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An array the guard will not let through.
///
/// The three fields are the three questions somebody debugging this asks in order: which rule, where
/// in the batch, and what the numbers were. A message that answers only the first is the reason
/// people stop reading error messages.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("{path} breaks the {invariant} rule: {detail}")]
pub struct Violation {
    /// The rule that was broken.
    pub invariant: Invariant,
    /// Where in the batch, as a dotted path of field names.
    pub path: String,
    /// The numbers, for whoever has to fix the decoder.
    pub detail: String,
}

impl Violation {
    /// A violation at a path.
    pub(crate) fn at(invariant: Invariant, path: &str, detail: impl Into<String>) -> Self {
        Self {
            invariant,
            path: if path.is_empty() {
                "the batch".to_owned()
            } else {
                path.to_owned()
            },
            detail: detail.into(),
        }
    }
}

/// What this crate returns.
pub type Result<T> = core::result::Result<T, Violation>;
