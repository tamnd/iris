//! Moving an Arrow schema in and out of the bytes a container carries.
//!
//! The container format does not depend on Arrow and does not look inside these bytes, which is
//! what keeps `iris-format` small enough to fuzz. That leaves somebody having to know what the
//! bytes mean, and this is where that somebody lives.

use std::fmt::Write as _;

use arrow_ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions, write_message};
use arrow_schema::Schema;

use crate::error::Result;

/// Reads an Arrow IPC schema message.
///
/// # Errors
///
/// Returns [`crate::Error::Arrow`] if the bytes are not a schema message.
pub fn schema_from_ipc(bytes: &[u8]) -> Result<Schema> {
    Ok(arrow_ipc::convert::try_schema_from_ipc_buffer(bytes)?)
}

/// Writes a schema as an Arrow IPC schema message, framed the way a reader expects.
///
/// This is what goes in the schema record of a container, and having it here means the thing that
/// writes a container and the thing that reads one cannot disagree about the framing.
///
/// # Errors
///
/// Returns [`crate::Error::Arrow`] if the schema cannot be encoded.
pub fn schema_to_ipc(schema: &Schema) -> Result<Vec<u8>> {
    let options = IpcWriteOptions::default();
    // No dictionaries are written here because a schema message carries none. The tracker exists
    // so the encoder has somewhere to record the ones it would have written, and refusing a
    // replacement is the strict setting, which is the right one for something with nothing to
    // replace.
    let mut tracker = DictionaryTracker::new(true);
    let encoded =
        IpcDataGenerator {}.schema_to_bytes_with_dictionary_tracker(schema, &mut tracker, &options);
    let mut out = Vec::new();
    write_message(&mut out, encoded, &options)?;
    Ok(out)
}

/// The columns of a schema, short enough to put in an error message.
///
/// An Arrow schema printed in full is pages of nested types, and a message that long gets truncated
/// by whatever ends up reading it. What somebody holding a dataset they cannot open actually needs
/// is enough to tell whether it is the dataset they were looking for, so this is the names and the
/// types in order, capped so that a schema with a thousand columns still produces a line a person
/// can read.
pub(crate) fn describe(schema: &Schema) -> String {
    const SHOWN: usize = 12;

    let fields = schema.fields();
    if fields.is_empty() {
        return "no columns at all".to_owned();
    }

    let mut out = fields
        .iter()
        .take(SHOWN)
        .map(|field| format!("{}: {}", field.name(), field.data_type()))
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(hidden) = fields.len().checked_sub(SHOWN).filter(|n| *n > 0) {
        let _ = write!(out, ", and {hidden} more");
    }
    out
}

#[cfg(test)]
mod tests {
    use arrow_schema::{DataType, Field};

    use super::{describe, schema_from_ipc, schema_to_ipc};

    #[test]
    fn a_schema_survives_the_round_trip() {
        let schema = arrow_schema::Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]);
        let bytes = schema_to_ipc(&schema).expect("a schema this small always encodes");
        let back = schema_from_ipc(&bytes).expect("what we just wrote is readable");
        assert_eq!(back, schema);
    }

    #[test]
    fn bytes_that_are_not_a_schema_are_an_error() {
        assert!(schema_from_ipc(b"not a schema at all").is_err());
    }

    #[test]
    fn a_description_names_every_column_and_its_type() {
        let schema = arrow_schema::Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]);
        assert_eq!(describe(&schema), "id: Int64, name: Utf8");
    }

    #[test]
    fn a_wide_schema_is_cut_short_rather_than_printed_whole() {
        let fields: Vec<_> = (0..40)
            .map(|i| Field::new(format!("c{i}"), DataType::Int64, false))
            .collect();
        let described = describe(&arrow_schema::Schema::new(fields));
        assert!(described.starts_with("c0: Int64, c1: Int64"));
        assert!(described.ends_with(", and 28 more"));
    }

    #[test]
    fn a_schema_with_no_columns_says_so() {
        assert_eq!(
            describe(&arrow_schema::Schema::empty()),
            "no columns at all"
        );
    }
}
