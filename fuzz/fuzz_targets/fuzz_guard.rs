//! The guard, against batches built to be almost right.
//!
//! The property under test is the one the guard promises: if it accepts a batch
//! then the arrays in that batch can be read without a read leaving the bytes it
//! was given. So this target builds every accepted batch with Arrow's own
//! validation turned off, which is the whole point. Building through
//! `ArrayData::try_new` would mean Arrow catching whatever the guard missed and
//! the target proving nothing about the guard. `build_unchecked` puts the guard
//! alone behind every read, and the sanitizer cargo-fuzz builds with is what
//! notices when it was wrong.
//!
//! Random bytes almost never describe a batch that gets as far as being
//! accepted, so the input is not read as a batch. It is read as instructions for
//! building a sound one, which is then corrupted a few times in ways a broken or
//! hostile decoder would produce: a length that no longer matches its buffer, a
//! buffer truncated by a byte, an offset with a bit flipped, a buffer missing
//! entirely. That keeps the fuzzer working near the boundary rather than a long
//! way outside it, where every input is refused for the same dull reason.
//!
//! `Utf8` and `LargeUtf8` are left out on purpose, and their absence costs no
//! coverage: they take the same layout arm and the same offset checks as
//! `Binary` and `LargeBinary`, which are in. What they also have is
//! `value(i)` reading through `from_utf8_unchecked`, so a column of arbitrary
//! bytes is undefined behaviour to read whatever the guard says. Encoding is
//! Arrow's half of the split this crate documents, and a host that skips Arrow's
//! pass has to keep checking it.

#![no_main]

use std::hint::black_box;
use std::sync::Arc;

use arbitrary::Unstructured;
use arrow_array::cast::AsArray;
use arrow_array::types::{ArrowPrimitiveType, Float64Type, Int8Type, Int32Type, Int64Type};
use arrow_array::{Array, FixedSizeBinaryArray, make_array};
use arrow_buffer::Buffer;
use arrow_data::{ArrayData, ArrayDataBuilder};
use arrow_schema::{DataType, Field, Fields, Schema};
use iris_abi::Node;
use libfuzzer_sys::fuzz_target;

/// How deep a generated schema nests, well inside the guard's own bound.
///
/// The bound itself is a schema question rather than a batch question and it has a test of its own.
/// What this number is for is keeping the generated batch small enough that the fuzzer runs many of
/// them a second.
const MAX_NEST: usize = 4;

/// The most rows a generated batch has before it is corrupted.
const MAX_ROWS: u64 = 64;

/// The longest array this target will build once the guard has accepted it.
///
/// Corruption deliberately produces lengths near `u64::MAX`, because the arms of the guard that
/// catch a length times a width overflowing are worth reaching. A column with no buffers at all,
/// `Null` being the obvious one, is accepted at any length, and there is nothing to read out of
/// bounds in an array with nothing in it. Building it would allocate for no reason, so past this
/// bound the target stops at the answer the guard gave.
const MAX_BUILD: u64 = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let mut source = Unstructured::new(data);
    let Ok(batch) = batch(&mut source) else {
        return;
    };

    if iris_guard::check(&batch.schema, batch.rows, &batch.nodes, &batch.buffers).is_err() {
        return;
    }

    if batch.nodes.iter().any(|node| node.length > MAX_BUILD) {
        return;
    }

    let mut reader = Reader {
        nodes: &batch.nodes,
        buffers: &batch.buffers,
        node: 0,
        buffer: 0,
    };
    for field in batch.schema.fields() {
        let array = make_array(reader.array(field));
        read(array.as_ref(), 0);
    }
});

/// A batch, as the guard takes one.
struct Batch {
    schema: Schema,
    rows: u64,
    nodes: Vec<Node>,
    buffers: Vec<Vec<u8>>,
}

/// Reads the input as a sound batch, then corrupts it.
fn batch(u: &mut Unstructured<'_>) -> arbitrary::Result<Batch> {
    let columns = u.int_in_range(0..=4usize)?;
    let rows = u.int_in_range(0..=MAX_ROWS)?;

    let mut fields = Vec::with_capacity(columns);
    for index in 0..columns {
        fields.push(Field::new(format!("c{index}"), data_type(u, 0)?, true));
    }
    let schema = Schema::new(fields);

    let mut builder = Builder {
        nodes: Vec::new(),
        buffers: Vec::new(),
    };
    for field in schema.fields() {
        builder.column(u, field.data_type(), rows)?;
    }

    let mut batch = Batch {
        schema,
        rows,
        nodes: builder.nodes,
        buffers: builder.buffers,
    };
    corrupt(u, &mut batch)?;
    Ok(batch)
}

/// One of the types the batch format carries, sometimes with something under it.
fn data_type(u: &mut Unstructured<'_>, depth: usize) -> arbitrary::Result<DataType> {
    let nested = if depth >= MAX_NEST { 8 } else { 11 };
    Ok(match u.int_in_range(0..=nested)? {
        0 => DataType::Null,
        1 => DataType::Boolean,
        2 => DataType::Int8,
        3 => DataType::Int32,
        4 => DataType::Int64,
        5 => DataType::Float64,
        6 => DataType::Binary,
        7 => DataType::LargeBinary,
        8 => DataType::FixedSizeBinary(u.int_in_range(1..=8)?),
        9 => DataType::List(item(u, depth)?),
        10 => DataType::FixedSizeList(item(u, depth)?, u.int_in_range(0..=4)?),
        _ => {
            let count = u.int_in_range(1..=3usize)?;
            let mut fields = Vec::with_capacity(count);
            for index in 0..count {
                fields.push(Field::new(format!("f{index}"), data_type(u, depth + 1)?, true));
            }
            DataType::Struct(Fields::from(fields))
        }
    })
}

/// The child of a list, under whatever name Arrow conventionally gives it.
fn item(u: &mut Unstructured<'_>, depth: usize) -> arbitrary::Result<Arc<Field>> {
    Ok(Arc::new(Field::new("item", data_type(u, depth + 1)?, true)))
}

/// Builds the two flat lists a batch carries, in the order the schema walks them.
struct Builder {
    nodes: Vec<Node>,
    buffers: Vec<Vec<u8>>,
}

impl Builder {
    /// One array and everything under it, sound by construction.
    fn column(
        &mut self,
        u: &mut Unstructured<'_>,
        data_type: &DataType,
        len: u64,
    ) -> arbitrary::Result<()> {
        let at = self.nodes.len();
        self.nodes.push(Node {
            length: len,
            null_count: 0,
        });

        if !matches!(data_type, DataType::Null) {
            let bitmap = validity(u, len)?;
            self.nodes[at].null_count = nulls_in(&bitmap, len);
            self.buffers.push(bitmap);
        }

        match data_type {
            DataType::Null => {}

            DataType::Boolean
            | DataType::Int8
            | DataType::Int32
            | DataType::Int64
            | DataType::Float64 => {
                let bytes = (len * slot_bits(data_type)).div_ceil(8);
                self.buffers.push(filled(u, bytes)?);
            }

            DataType::FixedSizeBinary(width) => {
                let width = u64::try_from(*width).unwrap_or(0);
                self.buffers.push(filled(u, len * width)?);
            }

            DataType::Binary | DataType::LargeBinary => {
                let bytes = u.int_in_range(0..=256u64)?;
                let wide = matches!(data_type, DataType::LargeBinary);
                self.buffers.push(offsets(u, len, bytes, wide)?);
                self.buffers.push(filled(u, bytes)?);
            }

            DataType::List(field) => {
                let child = u.int_in_range(0..=MAX_ROWS)?;
                self.buffers.push(offsets(u, len, child, false)?);
                self.column(u, field.data_type(), child)?;
            }

            DataType::FixedSizeList(field, size) => {
                let size = u64::try_from(*size).unwrap_or(0);
                self.column(u, field.data_type(), len * size)?;
            }

            DataType::Struct(fields) => {
                for field in fields {
                    self.column(u, field.data_type(), len)?;
                }
            }

            // `data_type` builds nothing else, so there is nothing else to build.
            _ => {}
        }

        Ok(())
    }
}

/// A validity bitmap, or nothing at all, which is how a column with no nulls says so.
fn validity(u: &mut Unstructured<'_>, len: u64) -> arbitrary::Result<Vec<u8>> {
    if u.arbitrary()? {
        return Ok(Vec::new());
    }
    filled(u, len.div_ceil(8))
}

/// How many of the first `len` bits of a bitmap are clear.
fn nulls_in(bitmap: &[u8], len: u64) -> u64 {
    if bitmap.is_empty() {
        return 0;
    }
    let clear = (0..len)
        .filter(|bit| {
            let byte = usize::try_from(bit / 8).unwrap_or(usize::MAX);
            bitmap.get(byte).is_none_or(|byte| byte >> (bit % 8) & 1 == 0)
        })
        .count();
    u64::try_from(clear).unwrap_or(0)
}

/// A run of `len + 1` offsets that climbs and stops inside `limit`.
fn offsets(
    u: &mut Unstructured<'_>,
    len: u64,
    limit: u64,
    wide: bool,
) -> arbitrary::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0u64;
    for index in 0..=len {
        if index > 0 {
            at += u.int_in_range(0..=(limit - at))?;
        }
        let offset = i64::try_from(at).unwrap_or(i64::MAX);
        if wide {
            out.extend_from_slice(&offset.to_le_bytes());
        } else {
            out.extend_from_slice(&i32::try_from(offset).unwrap_or(i32::MAX).to_le_bytes());
        }
    }
    Ok(out)
}

/// A buffer of the length asked for, filled from the input.
fn filled(u: &mut Unstructured<'_>, bytes: u64) -> arbitrary::Result<Vec<u8>> {
    let mut out = vec![0u8; usize::try_from(bytes).unwrap_or(0)];
    u.fill_buffer(&mut out)?;
    Ok(out)
}

/// How wide one slot of the fixed width types this target generates is, in bits.
fn slot_bits(data_type: &DataType) -> u64 {
    match data_type {
        DataType::Boolean => 1,
        DataType::Int8 => 8,
        DataType::Int32 => 32,
        _ => 64,
    }
}

/// Breaks the batch in one of the ways a decoder breaks one.
///
/// Every arm here is something the guard is supposed to catch, and the ones that reach for
/// `u64::MAX` are aimed at the arms that catch a multiplication rather than a comparison.
fn corrupt(u: &mut Unstructured<'_>, batch: &mut Batch) -> arbitrary::Result<()> {
    for _ in 0..u.int_in_range(0..=3u8)? {
        let choice = u.int_in_range(0..=6u8)?;
        match choice {
            0 | 1 if !batch.nodes.is_empty() => {
                let at = u.choose_index(batch.nodes.len())?;
                if choice == 0 {
                    batch.nodes[at].length = length(u)?;
                } else {
                    batch.nodes[at].null_count = length(u)?;
                }
            }
            2..=5 if !batch.buffers.is_empty() => {
                let at = u.choose_index(batch.buffers.len())?;
                match choice {
                    2 => {
                        let by = u.int_in_range(1..=8usize)?;
                        let keep = batch.buffers[at].len().saturating_sub(by);
                        batch.buffers[at].truncate(keep);
                    }
                    3 => {
                        let by = u.int_in_range(1..=8usize)?;
                        let grown = batch.buffers[at].len() + by;
                        batch.buffers[at].resize(grown, 0);
                    }
                    4 if !batch.buffers[at].is_empty() => {
                        let byte = u.choose_index(batch.buffers[at].len())?;
                        batch.buffers[at][byte] ^= 1 << u.int_in_range(0..=7u8)?;
                    }
                    _ => {
                        batch.buffers.remove(at);
                    }
                }
            }
            _ => batch.rows = length(u)?,
        }
    }
    Ok(())
}

/// A length, usually small and occasionally large enough to overflow a multiplication.
fn length(u: &mut Unstructured<'_>) -> arbitrary::Result<u64> {
    Ok(match u.int_in_range(0..=3u8)? {
        0 => u.int_in_range(0..=128u64)?,
        1 => u64::MAX,
        2 => u64::MAX / 2,
        _ => u64::from(u32::MAX),
    })
}

/// Builds arrays out of a batch the guard has already accepted.
///
/// This is the runtime's assembler with the checking taken out, which is the only interesting
/// version of it to fuzz.
struct Reader<'a> {
    nodes: &'a [Node],
    buffers: &'a [Vec<u8>],
    node: usize,
    buffer: usize,
}

impl Reader<'_> {
    fn array(&mut self, field: &Field) -> ArrayData {
        let node = self.nodes[self.node];
        self.node += 1;

        let data_type = field.data_type();
        let layout = iris_guard::layout(data_type, field.name()).expect("the guard walked this");
        let len = usize::try_from(node.length).expect("lengths are bounded above");

        let mut builder = ArrayDataBuilder::new(data_type.clone()).len(len);
        if layout.validity {
            let bytes = self.next();
            if !bytes.is_empty() {
                builder = builder
                    .null_bit_buffer(Some(Buffer::from_slice_ref(bytes)))
                    .null_count(usize::try_from(node.null_count).expect("counts are bounded"));
            }
        }
        for _ in 0..layout.values {
            builder = builder.add_buffer(Buffer::from_slice_ref(self.next()));
        }
        for child in layout.children {
            builder = builder.add_child_data(self.array(child));
        }

        // Nothing above this line checked anything. That is the point of the target: what stands
        // between these numbers and the reads below is the guard and nothing else.
        unsafe { builder.build_unchecked() }
    }

    fn next(&mut self) -> &[u8] {
        let bytes = &self.buffers[self.buffer];
        self.buffer += 1;
        bytes
    }
}

/// Reads every value of an array, and of everything under it.
fn read(array: &dyn Array, depth: usize) {
    if depth > MAX_NEST + 1 {
        return;
    }

    match array.data_type() {
        DataType::Null => {}
        DataType::Boolean => {
            let values = array.as_boolean();
            for index in indices(values.len()) {
                black_box(values.value(index));
            }
        }
        DataType::Int8 => primitive::<Int8Type>(array),
        DataType::Int32 => primitive::<Int32Type>(array),
        DataType::Int64 => primitive::<Int64Type>(array),
        DataType::Float64 => primitive::<Float64Type>(array),
        DataType::Binary => {
            let values = array.as_binary::<i32>();
            for index in indices(values.len()) {
                black_box(values.value(index).len());
            }
        }
        DataType::LargeBinary => {
            let values = array.as_binary::<i64>();
            for index in indices(values.len()) {
                black_box(values.value(index).len());
            }
        }
        DataType::FixedSizeBinary(_) => {
            let values = array
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .expect("the array was built from this type");
            for index in indices(values.len()) {
                black_box(values.value(index).len());
            }
        }
        DataType::List(_) => {
            let values = array.as_list::<i32>();
            for index in indices(values.len()) {
                read(values.value(index).as_ref(), depth + 1);
            }
        }
        DataType::FixedSizeList(_, _) => {
            let values = array.as_fixed_size_list();
            for index in indices(values.len()) {
                read(values.value(index).as_ref(), depth + 1);
            }
        }
        DataType::Struct(_) => {
            for column in array.as_struct().columns() {
                read(column.as_ref(), depth + 1);
            }
        }
        // Nothing else is generated, so nothing else arrives here.
        _ => {}
    }
}

/// Reads every slot of a fixed width column.
fn primitive<T: ArrowPrimitiveType>(array: &dyn Array) {
    let values = array.as_primitive::<T>();
    for index in indices(values.len()) {
        black_box(values.value(index));
    }
}

/// Which slots to read.
///
/// Everything, for anything small enough that reading it all is free. Past that the two ends, since
/// the slot an off by one reaches past is the last one and a long array is a long array because a
/// corruption made it one.
fn indices(len: usize) -> Vec<usize> {
    if len <= 64 {
        (0..len).collect()
    } else {
        (0..32).chain(len - 32..len).collect()
    }
}
