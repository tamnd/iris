//! The walk that decides whether a batch can be read.

use arrow_schema::{DataType, Field, Schema};
use iris_abi::Node;

use crate::error::{Invariant, Result, Violation};
use crate::layout::{Layout, layout, offset_width, slot_bits};

/// How deep a schema is allowed to nest.
///
/// The number is not interesting and the bound is. Everything downstream of this crate walks a
/// schema recursively, so a schema nested a hundred thousand deep is a stack overflow rather than an
/// error, and a stack overflow is not something a host can catch and turn into a failed query. Sixty
/// four is far past any schema anybody writes on purpose and far short of anything that threatens a
/// stack.
pub const MAX_DEPTH: usize = 64;

/// Checks a schema on its own, before anything is read against it.
///
/// This is separate from [`check`] because it is worth doing once when a dataset is opened rather
/// than once per batch, and because a host that is only inspecting a file still wants to know
/// whether it holds a type this build can carry.
///
/// # Errors
///
/// Returns a violation of [`Invariant::Depth`] if the schema nests past [`MAX_DEPTH`], or of
/// [`Invariant::Unsupported`] if it names a type this build cannot carry.
pub fn check_schema(schema: &Schema) -> Result<()> {
    // The walk is a worklist rather than a recursion, and that is the whole point of it. A
    // recursive depth check overflows the stack on exactly the input it exists to reject, which is
    // a check that works until it matters.
    let mut work: Vec<(&Field, usize, String)> = schema
        .fields()
        .iter()
        .map(|field| (field.as_ref(), 1, field.name().clone()))
        .collect();

    while let Some((field, depth, path)) = work.pop() {
        if depth > MAX_DEPTH {
            return Err(Violation::at(
                Invariant::Depth,
                &path,
                format!("this build walks {MAX_DEPTH} levels of nesting and this is deeper"),
            ));
        }
        let found = layout(field.data_type(), &path)?;
        for child in found.children {
            let child_path = format!("{path}.{}", child.name());
            work.push((child, depth + 1, child_path));
        }
    }

    Ok(())
}

/// Checks one batch against the schema it claims to be.
///
/// Everything here is a bounds question. If this returns `Ok` then every offset in the batch is
/// inside the buffer it indexes, every buffer is long enough for the number of slots its array
/// claims, and every array is long enough for the parent that points into it, so the arrays can be
/// read without a read going anywhere it should not.
///
/// What it deliberately does not check is whether the values mean anything. A `Utf8` column whose
/// bytes are not valid UTF-8 is refused later by Arrow, and it is refused there rather than here
/// because character encoding is a correctness property and not a bounds property: reading a badly
/// encoded string cannot leave the buffer. Splitting it that way keeps the fuzzed surface the one
/// with the silent failure mode.
///
/// # Errors
///
/// Returns the first violation found, naming the rule and the path.
pub fn check<B: AsRef<[u8]>>(
    schema: &Schema,
    rows: u64,
    nodes: &[Node],
    buffers: &[B],
) -> Result<()> {
    check_schema(schema)?;

    let mut cursor = Cursor {
        nodes,
        buffers,
        node: 0,
        buffer: 0,
    };

    for field in schema.fields() {
        let len = cursor.array(field, field.name())?;
        if len != rows {
            return Err(Violation::at(
                Invariant::Rows,
                field.name(),
                format!("the batch says {rows} rows and this column has {len}"),
            ));
        }
    }

    cursor.finish()
}

/// A position in the batch's two flat lists.
struct Cursor<'a, B> {
    nodes: &'a [Node],
    buffers: &'a [B],
    node: usize,
    buffer: usize,
}

impl<'a, B: AsRef<[u8]>> Cursor<'a, B> {
    /// Checks one array and everything under it, returning how long it is.
    fn array(&mut self, field: &Field, path: &str) -> Result<u64> {
        let node = self.next_node(path)?;
        let data_type = field.data_type();
        let Layout {
            validity,
            values,
            children,
        } = layout(data_type, path)?;

        if validity {
            let bitmap = self.next_buffer(path)?;
            check_validity(bitmap, node.length, node.null_count, path)?;
        } else if node.null_count != 0 {
            return Err(Violation::at(
                Invariant::NullCount,
                path,
                format!(
                    "a {data_type} column has no validity buffer and this one says it has {} nulls",
                    node.null_count
                ),
            ));
        }

        let mut taken = Vec::with_capacity(values);
        for _ in 0..values {
            taken.push(self.next_buffer(path)?);
        }

        // Children are walked before the parent's own buffers are checked against them, because a
        // list's offsets are only meaningful once its child's length is known.
        let mut child_lengths = Vec::with_capacity(children.len());
        for child in &children {
            let child_path = format!("{path}.{}", child.name());
            child_lengths.push(self.array(child, &child_path)?);
        }

        check_values(data_type, node.length, &taken, &child_lengths, path)?;
        Ok(node.length)
    }

    fn next_node(&mut self, path: &str) -> Result<Node> {
        let node = self.nodes.get(self.node).copied().ok_or_else(|| {
            Violation::at(
                Invariant::Arrays,
                path,
                format!(
                    "the schema calls for more arrays than the batch has, which is {}",
                    self.nodes.len()
                ),
            )
        })?;
        self.node += 1;
        Ok(node)
    }

    fn next_buffer(&mut self, path: &str) -> Result<&'a [u8]> {
        let bytes = self.buffers.get(self.buffer).ok_or_else(|| {
            Violation::at(
                Invariant::Buffers,
                path,
                format!(
                    "the schema calls for more buffers than the batch has, which is {}",
                    self.buffers.len()
                ),
            )
        })?;
        self.buffer += 1;
        Ok(bytes.as_ref())
    }

    /// Checks that the batch had nothing left over.
    ///
    /// A batch with spare arrays or spare buffers is not harmless. It means this host and the
    /// decoder disagree about the shape, and the next disagreement will be one where the counts
    /// happen to line up and the contents do not.
    fn finish(&self) -> Result<()> {
        if self.node != self.nodes.len() {
            return Err(Violation::at(
                Invariant::Arrays,
                "",
                format!(
                    "the batch has {} arrays and the schema accounts for {}",
                    self.nodes.len(),
                    self.node
                ),
            ));
        }
        if self.buffer != self.buffers.len() {
            return Err(Violation::at(
                Invariant::Buffers,
                "",
                format!(
                    "the batch has {} buffers and the schema accounts for {}",
                    self.buffers.len(),
                    self.buffer
                ),
            ));
        }
        Ok(())
    }
}

/// Checks a validity bitmap against the length and the null count that were declared alongside it.
///
/// An empty bitmap means every slot is present, which is how a decoder says a column has no nulls
/// without paying for a buffer of ones.
fn check_validity(bitmap: &[u8], len: u64, null_count: u64, path: &str) -> Result<()> {
    if bitmap.is_empty() {
        if null_count != 0 {
            return Err(Violation::at(
                Invariant::NullCount,
                path,
                format!(
                    "there is no validity buffer and this array says it has {null_count} nulls"
                ),
            ));
        }
        return Ok(());
    }

    let needed = len.div_ceil(8);
    let have = as_u64(bitmap.len());
    if have < needed {
        return Err(Violation::at(
            Invariant::Validity,
            path,
            format!("{len} slots need {needed} bytes of validity and there are {have}"),
        ));
    }

    // The null count is the one number in a batch that nothing else would catch. An array that says
    // it has no nulls and hands over a bitmap of zeroes produces wrong answers rather than an
    // error, which is the failure mode this whole crate exists for.
    let counted = count_nulls(bitmap, len);
    if counted != null_count {
        return Err(Violation::at(
            Invariant::NullCount,
            path,
            format!("this array says it has {null_count} nulls and its bitmap has {counted}"),
        ));
    }

    Ok(())
}

/// How many of the first `len` bits are clear.
fn count_nulls(bitmap: &[u8], len: u64) -> u64 {
    let mut nulls = 0;
    let mut seen = 0u64;
    for byte in bitmap {
        if seen >= len {
            break;
        }
        let left = len - seen;
        let bits = if left >= 8 {
            8
        } else {
            u32::try_from(left).unwrap_or(8)
        };
        let mask: u8 = if bits == 8 {
            u8::MAX
        } else {
            (1u8 << bits) - 1
        };
        nulls += u64::from(bits) - u64::from((byte & mask).count_ones());
        seen += u64::from(bits);
    }
    nulls
}

/// Checks the buffers an array takes for itself, once its children are known.
///
/// One arm per shape, because the shapes have nothing in common: a string's offsets are bounded by
/// a byte count, a list's by a row count, and a struct has no buffer of its own at all.
fn check_values(
    data_type: &DataType,
    len: u64,
    buffers: &[&[u8]],
    child_lengths: &[u64],
    path: &str,
) -> Result<()> {
    let child = child_lengths.first().copied().unwrap_or(0);

    match data_type {
        DataType::Null => Ok(()),
        DataType::Utf8 | DataType::Binary | DataType::LargeUtf8 | DataType::LargeBinary => {
            check_variable(data_type, len, buffers, path)
        }
        DataType::List(_) | DataType::LargeList(_) | DataType::Map(_, _) => {
            check_list(data_type, len, buffers, child, path)
        }
        DataType::FixedSizeList(_, size) => check_fixed_size_list(len, *size, child, path),
        DataType::Struct(fields) => {
            for (field, child) in fields.iter().zip(child_lengths) {
                if *child < len {
                    return Err(Violation::at(
                        Invariant::ChildLength,
                        path,
                        format!(
                            "this struct has {len} rows and its {} field has {child}",
                            field.name()
                        ),
                    ));
                }
            }
            Ok(())
        }
        // Everything left is fixed width, because `layout` refused anything else before this ran.
        other => check_fixed_width(other, len, buffers, path),
    }
}

/// A string or a binary column: offsets into a buffer of bytes.
fn check_variable(data_type: &DataType, len: u64, buffers: &[&[u8]], path: &str) -> Result<()> {
    let [offsets, data] = buffers else {
        return Err(counted_wrong(path, 2, buffers.len()));
    };
    let width = offset_width(data_type).expect("a variable length type has offsets");
    let last = check_offsets(offsets, len, width, path)?;
    let have = as_u64(data.len());
    if last > have {
        return Err(Violation::at(
            Invariant::OffsetRange,
            path,
            format!("the last offset is {last} and the values buffer is {have} bytes"),
        ));
    }
    Ok(())
}

/// A list or a map: offsets into a child array's slots rather than into bytes.
fn check_list(
    data_type: &DataType,
    len: u64,
    buffers: &[&[u8]],
    child: u64,
    path: &str,
) -> Result<()> {
    let [offsets] = buffers else {
        return Err(counted_wrong(path, 1, buffers.len()));
    };
    let width = offset_width(data_type).expect("a list has offsets");
    let last = check_offsets(offsets, len, width, path)?;
    if last > child {
        return Err(Violation::at(
            Invariant::OffsetRange,
            path,
            format!("the last offset is {last} and the child array has {child} slots"),
        ));
    }
    Ok(())
}

/// A fixed size list: no offsets at all, so the whole check is the multiplication.
fn check_fixed_size_list(len: u64, size: i32, child: u64, path: &str) -> Result<()> {
    let size = u64::try_from(size).map_err(|_| {
        Violation::at(
            Invariant::ChildLength,
            path,
            format!("a fixed size list cannot hold {size} values a row"),
        )
    })?;
    let needed = len.checked_mul(size).ok_or_else(|| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{len} rows of {size} values is more than this host can address"),
        )
    })?;
    if child < needed {
        return Err(Violation::at(
            Invariant::ChildLength,
            path,
            format!("{len} rows of {size} values need {needed} slots and the child has {child}"),
        ));
    }
    Ok(())
}

/// Everything whose slots are all the same width, which is most columns.
fn check_fixed_width(data_type: &DataType, len: u64, buffers: &[&[u8]], path: &str) -> Result<()> {
    let [values] = buffers else {
        return Err(counted_wrong(path, 1, buffers.len()));
    };
    let bits = slot_bits(data_type).ok_or_else(|| {
        Violation::at(
            Invariant::Unsupported,
            path,
            format!("this build does not know how wide a {data_type} slot is"),
        )
    })?;
    let needed = len
        .checked_mul(bits)
        .map(|total| total.div_ceil(8))
        .ok_or_else(|| {
            Violation::at(
                Invariant::Size,
                path,
                format!("{len} slots of {bits} bits is more than this host can address"),
            )
        })?;
    let have = as_u64(values.len());
    if have < needed {
        return Err(Violation::at(
            Invariant::BufferLength,
            path,
            format!("{len} slots of {bits} bits need {needed} bytes and there are {have}"),
        ));
    }
    Ok(())
}

/// Checks that a run of offsets is ordered, in range and long enough, and returns the last one.
///
/// The caller decides what the last offset has to be inside, because for a string it is a byte count
/// and for a list it is a row count, and the two are not the same question.
fn check_offsets(offsets: &[u8], len: u64, width: u64, path: &str) -> Result<u64> {
    // A zero length array is allowed to hand over no offsets at all. Arrow permits it and a decoder
    // that emits an empty batch should not have to allocate a buffer to say so.
    if len == 0 && offsets.is_empty() {
        return Ok(0);
    }

    let entries = len + 1;
    let needed = entries.checked_mul(width).ok_or_else(|| {
        Violation::at(
            Invariant::Size,
            path,
            format!("{entries} offsets of {width} bytes is more than this host can address"),
        )
    })?;
    let have = as_u64(offsets.len());
    if have < needed {
        return Err(Violation::at(
            Invariant::BufferLength,
            path,
            format!("{len} slots need {needed} bytes of offsets and there are {have}"),
        ));
    }

    let mut previous: i64 = 0;
    for index in 0..entries {
        let at = usize::try_from(index * width).map_err(|_| {
            Violation::at(
                Invariant::Size,
                path,
                "the offsets run past what this host can address".to_owned(),
            )
        })?;
        let offset = read_offset(offsets, at, width);

        if offset < 0 {
            return Err(Violation::at(
                Invariant::OffsetRange,
                path,
                format!("offset {index} is {offset}, and an offset is a position"),
            ));
        }
        if index > 0 && offset < previous {
            return Err(Violation::at(
                Invariant::OffsetOrder,
                path,
                format!("offset {index} is {offset} and the one before it is {previous}"),
            ));
        }
        previous = offset;
    }

    u64::try_from(previous).map_err(|_| {
        Violation::at(
            Invariant::OffsetRange,
            path,
            "the last offset is negative".to_owned(),
        )
    })
}

/// Reads one offset of the given width. The caller has already checked the buffer is long enough.
fn read_offset(bytes: &[u8], at: usize, width: u64) -> i64 {
    if width == 8 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[at..at + 8]);
        i64::from_le_bytes(raw)
    } else {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[at..at + 4]);
        i64::from(i32::from_le_bytes(raw))
    }
}

/// A length that came from a slice, which cannot be larger than a `u64` on any host iris runs on.
fn as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn counted_wrong(path: &str, wanted: usize, found: usize) -> Violation {
    Violation::at(
        Invariant::Buffers,
        path,
        format!("this column takes {wanted} buffers after its validity buffer and got {found}"),
    )
}

#[cfg(test)]
mod tests {
    use arrow_schema::{DataType, Field, Fields, Schema};
    use iris_abi::Node;

    use super::{MAX_DEPTH, check, check_schema, count_nulls};
    use crate::error::Invariant;

    fn node(length: u64, null_count: u64) -> Node {
        Node { length, null_count }
    }

    fn i64s(values: &[i64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn i32s(values: &[i32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn a_sound_batch_passes() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        let buffers = vec![Vec::new(), i64s(&[1, 2, 3])];
        check(&schema, 3, &[node(3, 0)], &buffers).expect("this batch is sound");
    }

    #[test]
    fn a_column_shorter_than_the_batch_is_caught() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        let buffers = vec![Vec::new(), i64s(&[1, 2])];
        let err = check(&schema, 3, &[node(2, 0)], &buffers).expect_err("two is not three");
        assert_eq!(err.invariant, Invariant::Rows);
    }

    #[test]
    fn a_values_buffer_one_slot_short_is_caught() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        let buffers = vec![Vec::new(), i64s(&[1, 2])];
        let err =
            check(&schema, 3, &[node(3, 0)], &buffers).expect_err("three slots need 24 bytes");
        assert_eq!(err.invariant, Invariant::BufferLength);
        assert!(err.to_string().contains("24 bytes"), "{err}");
    }

    #[test]
    fn a_bitmap_with_too_few_bits_is_caught() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, true)]);
        let buffers = vec![Vec::new(), i64s(&[1, 2, 3])];
        // An empty bitmap is fine, so this uses one that is present and too short.
        let short = vec![vec![0xffu8], i64s(&[1; 100])];
        let wide = Schema::new(vec![Field::new("a", DataType::Int64, true)]);
        let err = check(&wide, 100, &[node(100, 0)], &short).expect_err("100 slots need 13 bytes");
        assert_eq!(err.invariant, Invariant::Validity);
        check(&schema, 3, &[node(3, 0)], &buffers).expect("the empty bitmap case still passes");
    }

    #[test]
    fn an_offset_one_past_the_end_is_caught() {
        let schema = Schema::new(vec![Field::new("s", DataType::Utf8, false)]);
        let buffers = vec![Vec::new(), i32s(&[0, 2, 6]), b"hoyea".to_vec()];
        let err = check(&schema, 2, &[node(2, 0)], &buffers).expect_err("six is past five");
        assert_eq!(err.invariant, Invariant::OffsetRange);
    }

    #[test]
    fn offsets_that_run_backwards_are_caught() {
        let schema = Schema::new(vec![Field::new("s", DataType::Utf8, false)]);
        let buffers = vec![Vec::new(), i32s(&[0, 4, 2]), b"hoyea".to_vec()];
        let err = check(&schema, 2, &[node(2, 0)], &buffers).expect_err("two is less than four");
        assert_eq!(err.invariant, Invariant::OffsetOrder);
    }

    #[test]
    fn a_child_one_row_short_of_its_parent_is_caught() {
        let children = Fields::from(vec![Field::new("x", DataType::Int64, false)]);
        let schema = Schema::new(vec![Field::new("p", DataType::Struct(children), false)]);
        let buffers = vec![Vec::new(), Vec::new(), i64s(&[1, 2])];
        let err = check(&schema, 3, &[node(3, 0), node(2, 0)], &buffers)
            .expect_err("a struct's child cannot be shorter than the struct");
        assert_eq!(err.invariant, Invariant::ChildLength);
    }

    #[test]
    fn a_length_that_overflows_a_width_is_caught_rather_than_wrapped() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        let buffers = vec![Vec::new(), i64s(&[1])];
        let err = check(&schema, u64::MAX, &[node(u64::MAX, 0)], &buffers)
            .expect_err("that many slots is not addressable");
        assert_eq!(err.invariant, Invariant::Size);
    }

    #[test]
    fn a_schema_nested_past_the_bound_is_refused_without_recursing_into_it() {
        let mut data_type = DataType::Int64;
        for _ in 0..MAX_DEPTH + 10 {
            data_type = DataType::List(std::sync::Arc::new(Field::new("item", data_type, false)));
        }
        let schema = Schema::new(vec![Field::new("deep", data_type, false)]);
        let err = check_schema(&schema).expect_err("that is deeper than this build walks");
        assert_eq!(err.invariant, Invariant::Depth);
    }

    #[test]
    fn a_schema_at_the_bound_is_still_walked() {
        let mut data_type = DataType::Int64;
        for _ in 0..MAX_DEPTH - 1 {
            data_type = DataType::List(std::sync::Arc::new(Field::new("item", data_type, false)));
        }
        let schema = Schema::new(vec![Field::new("deep", data_type, false)]);
        check_schema(&schema).expect("this is exactly as deep as the bound allows");
    }

    #[test]
    fn spare_buffers_are_an_error_rather_than_something_ignored() {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        let buffers = vec![Vec::new(), i64s(&[1, 2, 3]), i64s(&[4])];
        let err =
            check(&schema, 3, &[node(3, 0)], &buffers).expect_err("a spare buffer is not fine");
        assert_eq!(err.invariant, Invariant::Buffers);
    }

    #[test]
    fn counting_nulls_stops_at_the_length_rather_than_the_byte() {
        // Five slots, all present, in a byte whose top three bits are clear.
        assert_eq!(count_nulls(&[0b0001_1111], 5), 0);
        assert_eq!(count_nulls(&[0b0001_1110], 5), 1);
        assert_eq!(count_nulls(&[0x00, 0xff], 9), 8);
    }
}
