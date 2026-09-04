//! What can go wrong while reading a container.

use iris_abi::Tag;

/// Why a container could not be read, or could not be trusted once it was read.
///
/// Every variant carries enough to put in a log line without going back to the file. An operator
/// who gets one of these should be able to say what is wrong with the dataset without opening a hex
/// editor, because the alternative is that they open a hex editor.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The file does not start with the container magic.
    #[error("this is not an iris container: it starts with {found:02x?} and not {expected:02x?}")]
    NotAContainer {
        /// The first eight bytes of the file, or fewer if the file is shorter than that.
        found: Vec<u8>,
        /// What those bytes should have been.
        expected: Vec<u8>,
    },
    /// The file starts correctly but ends somewhere unexpected.
    #[error("the container is truncated: {what} needs {needed} bytes and there are {available}")]
    Truncated {
        /// Which part of the file ran out.
        what: &'static str,
        /// How many bytes that part needed.
        needed: u64,
        /// How many were there.
        available: u64,
    },
    /// The container was written by a newer major version.
    #[error(
        "this container is format {major}.{minor} and this build reads format {supported_major}.x"
    )]
    UnsupportedFormat {
        /// The major version in the file.
        major: u16,
        /// The minor version in the file.
        minor: u16,
        /// The major version this build understands.
        supported_major: u16,
    },
    /// A reserved field was not zero.
    ///
    /// Reserved fields are the only place a future version can put a change that older readers must
    /// not ignore, so an older reader has to refuse rather than carry on.
    #[error(
        "a reserved field in the {what} is not zero, so this container uses something this build does not know about"
    )]
    Reserved {
        /// Which part of the container the field is in.
        what: &'static str,
    },
    /// A section points somewhere that is not inside the payload area.
    #[error(
        "section {id} covers bytes {offset}..{end} which is not inside the payload area of a {file_len} byte container"
    )]
    SectionOutOfBounds {
        /// Which section.
        id: u32,
        /// Where it claims to start.
        offset: u64,
        /// Where it claims to end, saturated at `u64::MAX`.
        end: u64,
        /// How long the file actually is.
        file_len: u64,
    },
    /// Two sections claim the same identifier.
    #[error("section id {id} is used twice, so a reference to it is ambiguous")]
    DuplicateSection {
        /// The repeated identifier.
        id: u32,
    },
    /// A required footer record was missing.
    #[error("the footer has no {0} record")]
    MissingRecord(Tag),
    /// A footer record appeared more times than it is allowed to.
    #[error("the footer has more than one {0} record")]
    RepeatedRecord(Tag),
    /// A known footer record was written at a version this build does not read.
    #[error("the footer has a {tag} record at version {version}, which this build does not read")]
    UnsupportedRecord {
        /// Which record.
        tag: Tag,
        /// The version on it.
        version: u16,
    },
    /// A digest did not match the bytes it covers.
    #[error(
        "the {what} digest does not match: the footer says {expected} and the bytes hash to {actual}"
    )]
    DigestMismatch {
        /// Which digest.
        what: String,
        /// What the container claims.
        expected: String,
        /// What the bytes actually hash to.
        actual: String,
    },
    /// The bytes inside the footer did not parse.
    #[error("the footer is malformed: {0}")]
    Footer(#[from] iris_abi::Error),
    /// Something in the container is bigger than this build can address.
    ///
    /// On a 64 bit host this cannot happen. It is here so that a 32 bit host gets a sentence
    /// instead of a panic.
    #[error(
        "the container declares {needed} bytes for {what}, which does not fit in this build's address space"
    )]
    TooLarge {
        /// Which part of the container.
        what: &'static str,
        /// How many bytes it asked for.
        needed: u64,
    },
}

/// The result of reading a container.
pub type Result<T> = core::result::Result<T, Error>;
