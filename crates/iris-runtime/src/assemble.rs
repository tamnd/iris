//! Turning a flat list of nodes and buffers into Arrow arrays.
//!
//! # What is left here
//!
//! Checking is not. Every structural question about a batch, how many arrays the schema calls for,
//! how many buffers each one takes, whether the offsets are inside the buffers they index, is
//! answered by `iris-guard` before a single array is built. This module is what happens after the
//! answer is yes.
//!
//! That split is not tidiness. The checks have to happen before anything walks the schema or
//! allocates against a length, and a module that both checks and builds ends up doing them in
//! whichever order the building needs.
//!
//! # Why the schema drives this and the batch does not
//!
//! A batch says how many rows it has and then hands over a flat list of nodes and a flat list of
//! buffers. It does not say what they are. The schema says what they are, and this walks the schema
//! in pre-order taking as many nodes and buffers as each field is entitled to.
//!
//! That is the same rule Arrow IPC follows, for the same reason: a batch that carries its own idea
//! of the shape and is believed is a much worse failure than one that disagrees with the schema and
//! is caught.
//!
//! # The copy
//!
//! Every buffer is copied again here, out of the `Vec` `iris-vm` took it into and into an Arrow
//! buffer. That is not gratuitous. Arrow reads a buffer of `i64` as `i64`, so the allocation has to
//! be aligned for it, and a `Vec<u8>` is aligned for `u8`. Fixing this properly means the guest
//! allocating into memory the host already owns, which is the same change that removes the first
//! copy, and it is not this milestone.
//!
//! # The second validation
//!
//! `ArrayData::try_new` validates what it is handed, so the structural checks run twice: once in the
//! guard, which produces the message and carries the rule that failed, and once in Arrow, which is
//! an implementation nobody here wrote. The cost of that is measured rather than assumed. Skipping
//! Arrow's pass means building arrays unchecked, which is an `unsafe` call this crate forbids and
//! would leave the guard's fuzzer as the only thing standing behind every read in the process.

use arrow_array::{RecordBatch, RecordBatchOptions, make_array};
use arrow_buffer::Buffer;
use arrow_data::ArrayData;
use arrow_schema::{Field, SchemaRef};
use iris_guard::Layout;
use iris_vm::RawBatch;

use crate::error::{Error, Result};

/// Assembles one batch against a schema.
///
/// # Errors
///
/// Returns [`Error::Guard`] if the batch does not describe the arrays the schema calls for or is
/// not structurally sound, [`Error::Shape`] if it is sound but larger than this host can hold, and
/// [`Error::Arrow`] if Arrow disagrees with the guard, which is a bug in one of them.
pub(crate) fn record_batch(schema: &SchemaRef, batch: &RawBatch) -> Result<RecordBatch> {
    // Nothing in `build` does any checking, so nothing in `build` runs until the batch has been
    // checked. The two halves are separate functions rather than one so that the cost of the first
    // one can be measured against the cost of the second, which is what the guard cost probe does.
    // They are only ever called together, and the order they are called in is this line.
    iris_guard::check(schema, batch.rows, &batch.nodes, &batch.buffers)?;
    build(schema, batch)
}

/// Builds the arrays for a batch that has already been checked.
///
/// # Errors
///
/// Returns [`Error::Shape`] if the batch is sound but larger than this host can hold, and
/// [`Error::Arrow`] if Arrow disagrees with the guard, which is a bug in one of them.
pub(crate) fn build(schema: &SchemaRef, batch: &RawBatch) -> Result<RecordBatch> {
    let rows = usize::try_from(batch.rows).map_err(|_| {
        Error::shape("a batch claims more rows than this host can hold in memory at once")
    })?;

    let mut cursor = Cursor {
        buffers: &batch.buffers,
        node: 0,
        buffer: 0,
        nodes: &batch.nodes,
    };

    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        columns.push(make_array(cursor.array(field)?));
    }

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
///
/// The guard has already walked these lists and found them to match the schema, so everything here
/// that could be missing is not missing. It is still read through the same accessors rather than
/// indexed directly, because a panic reached by disagreeing with the guard is the one failure mode
/// this crate must not have.
struct Cursor<'a> {
    nodes: &'a [iris_abi::Node],
    buffers: &'a [Vec<u8>],
    node: usize,
    buffer: usize,
}

impl Cursor<'_> {
    /// Builds one array and everything under it.
    fn array(&mut self, field: &Field) -> Result<ArrayData> {
        let data_type = field.data_type();
        let node = self.node()?;
        let len = usize::try_from(node.length).map_err(|_| {
            Error::shape("an array is longer than this host can hold in memory at once")
        })?;

        let Layout {
            validity,
            values,
            children,
        } = iris_guard::layout(data_type, field.name())?;

        let nulls = if validity { self.validity()? } else { None };

        let mut buffers = Vec::with_capacity(values);
        for _ in 0..values {
            buffers.push(Buffer::from_slice_ref(self.bytes()?));
        }

        let mut child_data = Vec::with_capacity(children.len());
        for child in children {
            child_data.push(self.array(child)?);
        }

        Ok(ArrayData::try_new(
            data_type.clone(),
            len,
            nulls,
            0,
            buffers,
            child_data,
        )?)
    }

    fn node(&mut self) -> Result<iris_abi::Node> {
        let node =
            self.nodes.get(self.node).copied().ok_or_else(|| {
                Error::shape("the guard and this module disagree about array counts")
            })?;
        self.node += 1;
        Ok(node)
    }

    fn bytes(&mut self) -> Result<&[u8]> {
        let bytes = self.buffers.get(self.buffer).ok_or_else(|| {
            Error::shape("the guard and this module disagree about buffer counts")
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
        assert!(matches!(err, Error::Guard(_)), "{err}");
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
        assert!(matches!(err, Error::Guard(_)), "{err}");
        assert!(err.to_string().contains("Dictionary"), "{err}");
    }
}
