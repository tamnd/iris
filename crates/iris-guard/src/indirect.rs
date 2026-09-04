//! The two arrays that do not hold their own values.
//!
//! A dictionary array holds keys into a dictionary that arrives separately, and a view array holds
//! views into data buffers that may not be the ones next to it. Both are an index into something
//! else, and an index into something else is where a checker earns its keep: the failure is not a
//! short buffer, it is a number that happens to be in range for the wrong thing.
//!
//! Neither type can be carried in a batch yet, and [`crate::layout`] refuses both by name. The
//! checks are written and tested here anyway, because the checks are the part that has to be right
//! and having them ready is what makes carrying these types later a format question rather than a
//! safety question.

use arrow_schema::DataType;

use crate::error::{Invariant, Result, Violation};

/// How wide one view is, in bytes.
///
/// Fixed by the Arrow specification: a four byte length, then either twelve bytes of inline data or
/// a four byte prefix, a buffer index and an offset.
pub const VIEW_WIDTH: usize = 16;

/// The longest value that fits inside a view rather than in a data buffer.
pub const VIEW_INLINE: u32 = 12;

/// Checks that every key is a slot in the dictionary.
///
/// The case worth naming is a key equal to the dictionary's length, which is in range for the
/// arithmetic and one past the end of the data, and is what an off by one in a decoder produces.
///
/// # Errors
///
/// Returns a violation of [`Invariant::DictionaryIndex`] for a key that is negative or not less
/// than `dictionary_len`, of [`Invariant::BufferLength`] if the keys buffer is short, and of
/// [`Invariant::Unsupported`] if the key type is not an integer.
pub fn check_dictionary(
    keys: &[u8],
    key_type: &DataType,
    len: u64,
    dictionary_len: u64,
    path: &str,
) -> Result<()> {
    let (width, signed) = match key_type {
        DataType::Int8 => (1usize, true),
        DataType::Int16 => (2, true),
        DataType::Int32 => (4, true),
        DataType::Int64 => (8, true),
        DataType::UInt8 => (1, false),
        DataType::UInt16 => (2, false),
        DataType::UInt32 => (4, false),
        DataType::UInt64 => (8, false),
        other => {
            return Err(Violation::at(
                Invariant::Unsupported,
                path,
                format!("{other} is not an integer, so it cannot be a dictionary key"),
            ));
        }
    };

    let slots = usize::try_from(len).map_err(|_| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{len} keys is more than this host can address"),
        )
    })?;
    let needed = slots.checked_mul(width).ok_or_else(|| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{len} keys of {width} bytes is more than this host can address"),
        )
    })?;
    if keys.len() < needed {
        return Err(Violation::at(
            Invariant::BufferLength,
            path,
            format!(
                "{len} keys need {needed} bytes and there are {}",
                keys.len()
            ),
        ));
    }

    for slot in 0..slots {
        let at = slot * width;
        let raw = &keys[at..at + width];
        let key = if signed {
            let value = read_signed(raw);
            if value < 0 {
                return Err(Violation::at(
                    Invariant::DictionaryIndex,
                    path,
                    format!("key {slot} is {value}, and a key is a position"),
                ));
            }
            u64::try_from(value).unwrap_or(u64::MAX)
        } else {
            read_unsigned(raw)
        };

        if key >= dictionary_len {
            return Err(Violation::at(
                Invariant::DictionaryIndex,
                path,
                format!("key {slot} is {key} and the dictionary has {dictionary_len} values"),
            ));
        }
    }

    Ok(())
}

/// Checks that every view points inside a data buffer that is actually there.
///
/// The case worth naming is a buffer index equal to the number of buffers, for the same reason as
/// the dictionary key: it is the value an off by one produces and it is in range for everything
/// except the thing it indexes.
///
/// # Errors
///
/// Returns a violation of [`Invariant::ViewBuffer`] for a view pointing at a buffer that is not
/// there or past the end of one that is, and of [`Invariant::BufferLength`] if the views buffer is
/// not a whole number of views or is short.
pub fn check_views<B: AsRef<[u8]>>(views: &[u8], data: &[B], len: u64, path: &str) -> Result<()> {
    let slots = usize::try_from(len).map_err(|_| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{len} views is more than this host can address"),
        )
    })?;
    let needed = slots.checked_mul(VIEW_WIDTH).ok_or_else(|| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{len} views of {VIEW_WIDTH} bytes is more than this host can address"),
        )
    })?;
    if views.len() < needed {
        return Err(Violation::at(
            Invariant::BufferLength,
            path,
            format!(
                "{len} views need {needed} bytes and there are {}",
                views.len()
            ),
        ));
    }

    for slot in 0..slots {
        let at = slot * VIEW_WIDTH;
        let view = &views[at..at + VIEW_WIDTH];
        let length = read_u32(&view[0..4]);

        // A short value lives in the view itself and points at nothing, so there is nothing here to
        // be out of range.
        if length <= VIEW_INLINE {
            continue;
        }

        let index = read_u32(&view[8..12]);
        let offset = read_u32(&view[12..16]);

        let buffer = usize::try_from(index)
            .ok()
            .and_then(|index| data.get(index))
            .ok_or_else(|| {
                Violation::at(
                    Invariant::ViewBuffer,
                    path,
                    format!(
                        "view {slot} points at data buffer {index} and there are {}",
                        data.len()
                    ),
                )
            })?;

        let end = u64::from(offset) + u64::from(length);
        let have = u64::try_from(buffer.as_ref().len()).unwrap_or(u64::MAX);
        if end > have {
            return Err(Violation::at(
                Invariant::ViewBuffer,
                path,
                format!(
                    "view {slot} reads {length} bytes at {offset} of data buffer {index}, which is \
                     {have} bytes"
                ),
            ));
        }
    }

    Ok(())
}

fn read_signed(raw: &[u8]) -> i64 {
    let mut wide = [0u8; 8];
    wide[..raw.len()].copy_from_slice(raw);
    let value = i64::from_le_bytes(wide);
    // Sign extend from the width that was actually read.
    let spare = 64 - u32::try_from(raw.len()).unwrap_or(8) * 8;
    (value << spare) >> spare
}

fn read_unsigned(raw: &[u8]) -> u64 {
    let mut wide = [0u8; 8];
    wide[..raw.len()].copy_from_slice(raw);
    u64::from_le_bytes(wide)
}

fn read_u32(raw: &[u8]) -> u32 {
    let mut wide = [0u8; 4];
    wide.copy_from_slice(raw);
    u32::from_le_bytes(wide)
}

#[cfg(test)]
mod tests {
    use arrow_schema::DataType;

    use super::{check_dictionary, check_views};
    use crate::error::Invariant;

    fn view(length: u32, index: u32, offset: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out
    }

    #[test]
    fn keys_inside_the_dictionary_pass() {
        let keys: Vec<u8> = [0i32, 1, 2].iter().flat_map(|k| k.to_le_bytes()).collect();
        check_dictionary(&keys, &DataType::Int32, 3, 3, "d").expect("every key is a slot");
    }

    #[test]
    fn a_key_equal_to_the_dictionary_length_is_caught() {
        let keys: Vec<u8> = [0i32, 3].iter().flat_map(|k| k.to_le_bytes()).collect();
        let err = check_dictionary(&keys, &DataType::Int32, 2, 3, "d")
            .expect_err("three is one past the last slot");
        assert_eq!(err.invariant, Invariant::DictionaryIndex);
        assert!(err.to_string().contains("has 3 values"), "{err}");
    }

    #[test]
    fn a_negative_key_is_caught_rather_than_read_as_enormous() {
        let keys: Vec<u8> = (-1i32).to_le_bytes().to_vec();
        let err =
            check_dictionary(&keys, &DataType::Int32, 1, 3, "d").expect_err("a key is a position");
        assert_eq!(err.invariant, Invariant::DictionaryIndex);
        assert!(err.to_string().contains("is -1"), "{err}");
    }

    #[test]
    fn an_unsigned_key_type_is_read_as_unsigned() {
        let keys: Vec<u8> = 200u8.to_le_bytes().to_vec();
        check_dictionary(&keys, &DataType::UInt8, 1, 255, "d").expect("200 is a slot in 255");
        let err = check_dictionary(&keys, &DataType::UInt8, 1, 200, "d")
            .expect_err("200 is not a slot in 200");
        assert_eq!(err.invariant, Invariant::DictionaryIndex);
    }

    #[test]
    fn a_key_type_that_is_not_an_integer_is_refused_by_name() {
        let err = check_dictionary(&[], &DataType::Utf8, 0, 0, "d")
            .expect_err("a string is not a key type");
        assert_eq!(err.invariant, Invariant::Unsupported);
    }

    #[test]
    fn views_inside_their_buffers_pass() {
        let data: Vec<Vec<u8>> = vec![b"hello there friend".to_vec()];
        let views = [view(18, 0, 0), view(4, 0, 3)].concat();
        check_views(&views, &data, 2, "v").expect("both views are inside the buffer");
    }

    #[test]
    fn a_view_buffer_index_equal_to_the_buffer_count_is_caught() {
        let data: Vec<Vec<u8>> = vec![b"hello there friend".to_vec()];
        let views = view(18, 1, 0);
        let err = check_views(&views, &data, 1, "v").expect_err("there is no buffer 1");
        assert_eq!(err.invariant, Invariant::ViewBuffer);
        assert!(err.to_string().contains("there are 1"), "{err}");
    }

    #[test]
    fn a_view_that_runs_off_the_end_of_its_buffer_is_caught() {
        let data: Vec<Vec<u8>> = vec![b"hello there friend".to_vec()];
        let views = view(18, 0, 1);
        let err = check_views(&views, &data, 1, "v").expect_err("that is one byte too many");
        assert_eq!(err.invariant, Invariant::ViewBuffer);
    }

    #[test]
    fn a_short_value_lives_in_the_view_and_points_at_nothing() {
        let data: Vec<Vec<u8>> = Vec::new();
        let views = view(12, 99, 99);
        check_views(&views, &data, 1, "v").expect("an inline value indexes nothing");
    }

    #[test]
    fn a_views_buffer_that_is_short_is_caught() {
        let data: Vec<Vec<u8>> = Vec::new();
        let err = check_views(&[0u8; 8], &data, 1, "v").expect_err("a view is sixteen bytes");
        assert_eq!(err.invariant, Invariant::BufferLength);
    }
}
