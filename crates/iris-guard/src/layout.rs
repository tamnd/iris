//! How many buffers and children each Arrow type takes.
//!
//! This is a transcription of the Arrow specification's layout section, and it lives here rather
//! than next to the code that assembles arrays because two copies of it would eventually disagree,
//! and the copy that disagrees silently is the one in the checker.
//!
//! | Type | Validity | Then | Children |
//! | --- | --- | --- | --- |
//! | Null | no | nothing | none |
//! | Boolean, the fixed width types, `FixedSizeBinary` | yes | values | none |
//! | `Utf8`, `Binary` and their large forms | yes | offsets, values | none |
//! | `List`, `LargeList`, `Map` | yes | offsets | one |
//! | `FixedSizeList` | yes | nothing | one |
//! | `Struct` | yes | nothing | one per field |
//!
//! Unions, dictionaries, run end encoding and the view types are not here yet. Each of them needs
//! something the batch this crate checks cannot carry: a union has a type id map and no validity
//! buffer, a dictionary needs its values to arrive out of band, and a view array has a variable
//! number of data buffers, which is the one thing that breaks counting buffers against a schema.
//! They are refused by name rather than skipped, because a column that quietly does not arrive is
//! worse than one that fails.
//!
//! The two checks those types need are written and tested even so, in [`crate::check_dictionary`]
//! and [`crate::check_views`], because the checks are the hard part and having them ready is what
//! makes carrying the types a format question rather than a safety question.

use arrow_schema::{DataType, Field};

use crate::error::{Invariant, Result, Violation};

/// What one array takes out of a batch.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layout<'a> {
    /// Whether the array has a validity buffer. Only `Null` does not.
    pub validity: bool,
    /// How many buffers follow the validity one.
    pub values: usize,
    /// The children, in order.
    pub children: Vec<&'a Field>,
}

/// The layout of one type.
///
/// # Errors
///
/// Returns a [`Invariant::Unsupported`] violation for a type this crate cannot check, which is the
/// same thing as a type iris cannot carry.
pub fn layout<'a>(data_type: &'a DataType, path: &str) -> Result<Layout<'a>> {
    let (validity, values, children) = match data_type {
        DataType::Null => (false, 0, Vec::new()),

        DataType::Boolean
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Date32
        | DataType::Date64
        | DataType::Time32(_)
        | DataType::Time64(_)
        | DataType::Timestamp(_, _)
        | DataType::Duration(_)
        | DataType::Interval(_)
        | DataType::Decimal32(_, _)
        | DataType::Decimal64(_, _)
        | DataType::Decimal128(_, _)
        | DataType::Decimal256(_, _)
        | DataType::FixedSizeBinary(_) => (true, 1, Vec::new()),

        DataType::Utf8 | DataType::Binary | DataType::LargeUtf8 | DataType::LargeBinary => {
            (true, 2, Vec::new())
        }

        DataType::List(field) | DataType::LargeList(field) | DataType::Map(field, _) => {
            (true, 1, vec![field.as_ref()])
        }

        DataType::FixedSizeList(field, _) => (true, 0, vec![field.as_ref()]),

        DataType::Struct(fields) => (true, 0, fields.iter().map(AsRef::as_ref).collect()),

        other => {
            return Err(Violation::at(
                Invariant::Unsupported,
                path,
                format!("this build does not carry {other} columns"),
            ));
        }
    };

    Ok(Layout {
        validity,
        values,
        children,
    })
}

/// How wide one slot of a fixed width type is, in bits.
///
/// Bits rather than bytes because of `Boolean`, which is the whole reason this returns a number that
/// has to be divided rather than multiplied.
pub(crate) fn slot_bits(data_type: &DataType) -> Option<u64> {
    let bits = match data_type {
        DataType::Boolean => 1,
        DataType::Int8 | DataType::UInt8 => 8,
        DataType::Int16 | DataType::UInt16 | DataType::Float16 => 16,
        DataType::Int32
        | DataType::UInt32
        | DataType::Float32
        | DataType::Date32
        | DataType::Time32(_)
        | DataType::Decimal32(_, _) => 32,
        DataType::Int64
        | DataType::UInt64
        | DataType::Float64
        | DataType::Date64
        | DataType::Time64(_)
        | DataType::Timestamp(_, _)
        | DataType::Duration(_)
        | DataType::Decimal64(_, _) => 64,
        DataType::Decimal128(_, _) => 128,
        DataType::Decimal256(_, _) => 256,
        DataType::Interval(unit) => match unit {
            arrow_schema::IntervalUnit::YearMonth => 32,
            arrow_schema::IntervalUnit::DayTime => 64,
            arrow_schema::IntervalUnit::MonthDayNano => 128,
        },
        // A negative width is not a width. Arrow's own type carries an `i32` and nothing stops a
        // schema from declaring one, so it is turned down here rather than cast.
        DataType::FixedSizeBinary(width) => u64::try_from(*width).ok()? * 8,
        _ => return None,
    };
    Some(bits)
}

/// How wide the offsets of a variable length type are, in bytes.
pub(crate) const fn offset_width(data_type: &DataType) -> Option<u64> {
    match data_type {
        DataType::Utf8 | DataType::Binary | DataType::List(_) | DataType::Map(_, _) => Some(4),
        DataType::LargeUtf8 | DataType::LargeBinary | DataType::LargeList(_) => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use arrow_schema::{DataType, Field, Fields};

    use super::{layout, offset_width, slot_bits};
    use crate::error::Invariant;

    #[test]
    fn a_flat_column_takes_a_validity_buffer_and_a_values_buffer() {
        let found = layout(&DataType::Int64, "a").expect("Int64 is a type iris carries");
        assert!(found.validity);
        assert_eq!(found.values, 1);
        assert!(found.children.is_empty());
    }

    #[test]
    fn a_struct_takes_no_values_buffer_and_one_child_per_field() {
        let fields = Fields::from(vec![
            Field::new("x", DataType::Int64, false),
            Field::new("y", DataType::Int64, false),
        ]);
        let point = DataType::Struct(fields);
        let found = layout(&point, "p").expect("Struct is a type iris carries");
        assert_eq!(found.values, 0);
        assert_eq!(found.children.len(), 2);
    }

    #[test]
    fn null_is_the_one_type_with_no_validity_buffer() {
        let found = layout(&DataType::Null, "n").expect("Null is a type iris carries");
        assert!(!found.validity);
        assert_eq!(found.values, 0);
    }

    #[test]
    fn a_type_this_build_does_not_carry_is_refused_by_name() {
        let dictionary = DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));
        let err = layout(&dictionary, "d").expect_err("a dictionary is not carried yet");
        assert_eq!(err.invariant, Invariant::Unsupported);
        assert!(err.to_string().contains("Dictionary"), "{err}");
    }

    #[test]
    fn a_boolean_slot_is_one_bit_and_a_timestamp_is_sixty_four() {
        assert_eq!(slot_bits(&DataType::Boolean), Some(1));
        assert_eq!(
            slot_bits(&DataType::Timestamp(
                arrow_schema::TimeUnit::Nanosecond,
                None
            )),
            Some(64)
        );
        assert_eq!(slot_bits(&DataType::FixedSizeBinary(7)), Some(56));
    }

    #[test]
    fn a_negative_fixed_width_is_not_a_width() {
        assert_eq!(slot_bits(&DataType::FixedSizeBinary(-1)), None);
    }

    #[test]
    fn the_large_variants_are_the_ones_with_eight_byte_offsets() {
        assert_eq!(offset_width(&DataType::Utf8), Some(4));
        assert_eq!(offset_width(&DataType::LargeUtf8), Some(8));
        assert_eq!(offset_width(&DataType::Int64), None);
    }
}
