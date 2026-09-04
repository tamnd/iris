//! Turning a flat list of nodes and buffers into Arrow arrays.
//!
//! # Why the schema drives this and the batch does not
//!
//! A batch says how many rows it has and then hands over a flat list of nodes and a flat list of
//! buffers. It does not say what they are. The schema says what they are, and this walks the schema
//! in pre-order taking as many nodes and buffers as each field is entitled to. A decoder that hands
//! over the wrong number of either runs out or has some left over, and both are caught here by
//! counting rather than by trusting.
//!
//! That is the same rule Arrow IPC follows, for the same reason: a batch that carries its own idea
//! of the shape and is believed is a much worse failure than one that disagrees with the schema and
//! is caught.
//!
//! # The buffer table
//!
//! How many buffers a field takes is fixed by the Arrow specification, and this is a transcription
//! of it. Every array starts with a validity buffer, except the two that the specification says do
//! not have one.
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
//! Unions, dictionaries, run end encoding and the view types are not here. They are all real and
//! they all need something this milestone does not have: a union has no validity buffer and a type
//! id map, a dictionary needs its values to arrive out of band, and a view array has a variable
//! number of data buffers, which is the one thing that breaks counting. Each of them is refused by
//! name rather than skipped.
//!
//! # The copy
//!
//! Every buffer is copied again here, out of the `Vec` `iris-vm` took it into and into an Arrow
//! buffer. That is not gratuitous. Arrow reads a buffer of `i64` as `i64`, so the allocation has to
//! be aligned for it, and a `Vec<u8>` is aligned for `u8`. Fixing this properly means the guest
//! allocating into memory the host already owns, which is the same change that removes the first
//! copy, and it is not this milestone.

use arrow_array::{RecordBatch, RecordBatchOptions, make_array};
use arrow_buffer::Buffer;
use arrow_data::ArrayData;
use arrow_schema::{DataType, SchemaRef};
use iris_abi::Node;
use iris_vm::RawBatch;

use crate::error::{Error, Result};

/// Assembles one batch against a schema.
///
/// # Errors
///
/// Returns [`Error::Shape`] if the batch does not describe the arrays the schema calls for,
/// [`Error::Unsupported`] if the schema uses a type this build cannot assemble, and
/// [`Error::Arrow`] if the buffers do not form a valid array.
pub(crate) fn record_batch(schema: &SchemaRef, batch: &RawBatch) -> Result<RecordBatch> {
    let rows = usize::try_from(batch.rows).map_err(|_| {
        Error::shape("a batch claims more rows than this host can hold in memory at once")
    })?;

    let mut cursor = Cursor {
        nodes: &batch.nodes,
        buffers: &batch.buffers,
        node: 0,
        buffer: 0,
    };

    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let data = cursor.array(field.data_type())?;
        if data.len() != rows {
            return Err(Error::shape(format!(
                "the batch says {rows} rows and column {} has {}",
                field.name(),
                data.len()
            )));
        }
        columns.push(make_array(data));
    }
    cursor.finish()?;

    // The row count is passed explicitly so that a schema with no fields still produces a batch
    // with the right number of rows. `count(*)` over a projection of nothing is a real query.
    let options = RecordBatchOptions::new().with_row_count(Some(rows));
    Ok(RecordBatch::try_new_with_options(
        schema.clone(),
        columns,
        &options,
    )?)
}

/// A position in the batch's two flat lists.
struct Cursor<'a> {
    nodes: &'a [Node],
    buffers: &'a [Vec<u8>],
    node: usize,
    buffer: usize,
}

impl Cursor<'_> {
    /// Builds one array and everything under it, taking what it is entitled to and no more.
    fn array(&mut self, data_type: &DataType) -> Result<ArrayData> {
        let node = self.node()?;
        let len = usize::try_from(node.length).map_err(|_| {
            Error::shape("an array is longer than this host can hold in memory at once")
        })?;

        let (values, children) = layout(data_type)?;

        // Null and union are the two types the Arrow specification gives no validity buffer, and
        // union is refused above, so this is the only exception that reaches here.
        let validity = if matches!(data_type, DataType::Null) {
            None
        } else {
            self.validity()?
        };

        let mut buffers = Vec::with_capacity(values);
        for _ in 0..values {
            buffers.push(Buffer::from_slice_ref(self.bytes()?));
        }

        let mut child_data = Vec::with_capacity(children.len());
        for child in children {
            child_data.push(self.array(child)?);
        }

        let data = ArrayData::try_new(data_type.clone(), len, validity, 0, buffers, child_data)?;

        // The node's null count is not used to build anything, which makes it the one field in a
        // batch that nothing would otherwise check. A decoder that says an array has no nulls and
        // hands over a validity buffer full of zeroes is broken in a way that produces wrong
        // answers rather than an error, so it is checked here against what the bitmap says.
        let counted = u64::try_from(data.null_count()).unwrap_or(u64::MAX);
        if counted != node.null_count {
            return Err(Error::shape(format!(
                "an array says it has {} nulls and its validity buffer has {counted}",
                node.null_count
            )));
        }

        Ok(data)
    }

    fn node(&mut self) -> Result<Node> {
        let node = self.nodes.get(self.node).copied().ok_or_else(|| {
            Error::shape(format!(
                "the schema calls for more arrays than the batch has, which has {}",
                self.nodes.len()
            ))
        })?;
        self.node += 1;
        Ok(node)
    }

    fn bytes(&mut self) -> Result<&[u8]> {
        let bytes = self.buffers.get(self.buffer).ok_or_else(|| {
            Error::shape(format!(
                "the schema calls for more buffers than the batch has, which has {}",
                self.buffers.len()
            ))
        })?;
        self.buffer += 1;
        Ok(bytes)
    }

    /// The validity buffer, where empty means every value is present.
    ///
    /// The entry is always there even when it is empty, because the schema decides how many buffers
    /// an array has and leaving one out would shift every buffer after it.
    fn validity(&mut self) -> Result<Option<Buffer>> {
        let bytes = self.bytes()?;
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Buffer::from_slice_ref(bytes)))
        }
    }

    /// Checks that the batch had nothing left over.
    fn finish(&self) -> Result<()> {
        if self.node != self.nodes.len() {
            return Err(Error::shape(format!(
                "the batch has {} arrays and the schema accounts for {}",
                self.nodes.len(),
                self.node
            )));
        }
        if self.buffer != self.buffers.len() {
            return Err(Error::shape(format!(
                "the batch has {} buffers and the schema accounts for {}",
                self.buffers.len(),
                self.buffer
            )));
        }
        Ok(())
    }
}

/// How many buffers after the validity one, and which children, a type has.
///
/// This is the table in the module documentation, in the form the code uses it.
fn layout(data_type: &DataType) -> Result<(usize, Vec<&DataType>)> {
    let layout = match data_type {
        DataType::Null => (0, Vec::new()),

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
        | DataType::FixedSizeBinary(_) => (1, Vec::new()),

        DataType::Utf8 | DataType::Binary | DataType::LargeUtf8 | DataType::LargeBinary => {
            (2, Vec::new())
        }

        DataType::List(field) | DataType::LargeList(field) | DataType::Map(field, _) => {
            (1, vec![field.data_type()])
        }

        DataType::FixedSizeList(field, _) => (0, vec![field.data_type()]),

        DataType::Struct(fields) => (0, fields.iter().map(|f| f.data_type()).collect()),

        other => return Err(Error::Unsupported(other.to_string())),
    };
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Array, Int64Array, StringArray, StructArray};
    use arrow_schema::{DataType, Field, Fields, Schema};
    use iris_abi::Node;
    use iris_vm::RawBatch;

    use super::record_batch;
    use crate::error::Error;

    fn i64_column(values: &[i64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn a_flat_batch_of_integers_becomes_a_record_batch() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch = RawBatch {
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), i64_column(&[7, 8, 9])],
        };

        let out = record_batch(&schema, &batch).expect("this batch matches its schema");
        let column = out
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("an Int64 field produces an Int64Array");
        assert_eq!(column.values(), &[7, 8, 9]);
    }

    #[test]
    fn a_string_column_takes_three_buffers() {
        let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, false)]));
        let offsets: Vec<u8> = [0i32, 2, 5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        let batch = RawBatch {
            rows: 2,
            nodes: vec![Node {
                length: 2,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), offsets, b"hoyeah".to_vec()],
        };

        let out = record_batch(&schema, &batch).expect("this batch matches its schema");
        let column = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("a Utf8 field produces a StringArray");
        assert_eq!(column.value(0), "ho");
        assert_eq!(column.value(1), "yea");
    }

    #[test]
    fn a_struct_takes_a_node_for_itself_and_one_per_child() {
        let children = Fields::from(vec![
            Field::new("x", DataType::Int64, false),
            Field::new("y", DataType::Int64, false),
        ]);
        let schema = Arc::new(Schema::new(vec![Field::new(
            "point",
            DataType::Struct(children),
            false,
        )]));
        let batch = RawBatch {
            rows: 2,
            nodes: vec![
                Node {
                    length: 2,
                    null_count: 0,
                },
                Node {
                    length: 2,
                    null_count: 0,
                },
                Node {
                    length: 2,
                    null_count: 0,
                },
            ],
            buffers: vec![
                Vec::new(),
                Vec::new(),
                i64_column(&[1, 2]),
                Vec::new(),
                i64_column(&[3, 4]),
            ],
        };

        let out = record_batch(&schema, &batch).expect("this batch matches its schema");
        let column = out
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("a Struct field produces a StructArray");
        assert_eq!(column.num_columns(), 2);
        assert_eq!(column.len(), 2);
    }

    #[test]
    fn a_batch_with_too_few_buffers_is_named_rather_than_guessed_at() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch = RawBatch {
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 0,
            }],
            buffers: vec![Vec::new()],
        };

        let err = record_batch(&schema, &batch).expect_err("a missing values buffer is not fine");
        assert!(matches!(err, Error::Shape(_)), "{err}");
        assert!(err.to_string().contains("more buffers than the batch has"));
    }

    #[test]
    fn a_batch_with_buffers_left_over_is_an_error() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch = RawBatch {
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), i64_column(&[7, 8, 9]), i64_column(&[1])],
        };

        let err = record_batch(&schema, &batch).expect_err("a spare buffer is not fine either");
        assert!(err.to_string().contains("the schema accounts for"), "{err}");
    }

    #[test]
    fn a_null_count_that_disagrees_with_the_bitmap_is_caught() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, true)]));
        let batch = RawBatch {
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 2,
            }],
            // Every bit set, so nothing is null, which is not what the node says.
            buffers: vec![vec![0xff], i64_column(&[7, 8, 9])],
        };

        let err = record_batch(&schema, &batch).expect_err("a lie about nulls is not fine");
        assert!(err.to_string().contains("nulls"), "{err}");
    }

    #[test]
    fn a_type_this_build_cannot_assemble_is_refused_by_name() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "d",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            false,
        )]));
        let batch = RawBatch {
            rows: 0,
            nodes: vec![Node {
                length: 0,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), Vec::new()],
        };

        let err = record_batch(&schema, &batch).expect_err("a dictionary is not supported yet");
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("Dictionary"), "{err}");
    }
}
