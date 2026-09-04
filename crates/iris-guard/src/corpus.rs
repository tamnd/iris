//! The adversarial corpus, in the repository so it can be extended.
//!
//! Seven of these came out of reading what the prior art does not check. They are here rather than
//! in a test file because they are useful to anybody writing a host or a decoder against this ABI,
//! and because a corpus that lives in a test file gets extended by whoever is already editing that
//! file, which is nobody.
//!
//! The sound cases matter as much as the unsound ones. A checker that refuses everything passes
//! every adversarial corpus ever written.

use arrow_schema::{DataType, Field, Fields, Schema};
use iris_abi::Node;

use crate::check::{MAX_DEPTH, check};
use crate::error::{Invariant, Result};
use crate::indirect::{check_dictionary, check_views};

/// One case, and what the guard is supposed to say about it.
#[derive(Clone, Debug)]
pub struct Case {
    /// What the case is called, in prose.
    pub name: &'static str,
    /// Why it is in the corpus, which is usually a decoder bug it stands in for.
    pub why: &'static str,
    /// The rule it should break, or `None` if the guard has to accept it.
    pub expected: Option<Invariant>,
    /// The thing to run.
    pub subject: Subject,
}

/// What a case hands the guard.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Subject {
    /// A whole batch against a schema.
    Batch {
        /// What the batch claims to be.
        schema: Schema,
        /// How many rows the batch claims.
        rows: u64,
        /// One entry per array.
        nodes: Vec<Node>,
        /// One entry per buffer.
        buffers: Vec<Vec<u8>>,
    },
    /// Dictionary keys against a dictionary of a given size.
    Dictionary {
        /// The encoded keys.
        keys: Vec<u8>,
        /// The type one key is stored as.
        key_type: DataType,
        /// How many keys there are.
        len: u64,
        /// How many values the dictionary holds.
        dictionary_len: u64,
    },
    /// Views against the data buffers they point into.
    Views {
        /// The encoded views, sixteen bytes each.
        views: Vec<u8>,
        /// The data buffers.
        data: Vec<Vec<u8>>,
        /// How many views there are.
        len: u64,
    },
}

impl Subject {
    /// Runs the case through the guard.
    ///
    /// # Errors
    ///
    /// Returns whatever the guard says, which for most of this corpus is the point.
    pub fn run(&self) -> Result<()> {
        match self {
            Self::Batch {
                schema,
                rows,
                nodes,
                buffers,
            } => check(schema, *rows, nodes, buffers),
            Self::Dictionary {
                keys,
                key_type,
                len,
                dictionary_len,
            } => check_dictionary(keys, key_type, *len, *dictionary_len, "keys"),
            Self::Views { views, data, len } => check_views(views, data, *len, "views"),
        }
    }
}

/// Every case in the corpus.
#[must_use]
pub fn cases() -> Vec<Case> {
    vec![
        sound_integers(),
        sound_strings_with_nulls(),
        sound_nested_struct(),
        offset_one_past_the_end(),
        null_count_off_by_one(),
        dictionary_index_equal_to_the_dictionary_length(),
        view_buffer_index_equal_to_the_buffer_count(),
        length_times_width_that_overflows(),
        child_one_row_short_of_its_parent(),
        schema_nesting_without_a_bound(),
    ]
}

fn i64s(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn i32s(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn sound_integers() -> Case {
    Case {
        name: "three integers",
        why: "a checker that refuses everything passes every corpus, so the corpus has to include \
              batches that are fine",
        expected: None,
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("a", DataType::Int64, false)]),
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), i64s(&[1, 2, 3])],
        },
    }
}

fn sound_strings_with_nulls() -> Case {
    Case {
        name: "two strings, one of them null",
        why: "the validity path and the offsets path are the two that get the most attention here, \
              so both need a case that passes",
        expected: None,
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("s", DataType::Utf8, true)]),
            rows: 2,
            nodes: vec![Node {
                length: 2,
                null_count: 1,
            }],
            // The first slot is present and the second is null, so the second string is empty.
            buffers: vec![vec![0b0000_0001], i32s(&[0, 2, 2]), b"ho".to_vec()],
        },
    }
}

fn sound_nested_struct() -> Case {
    let children = Fields::from(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Int64, false),
    ]);
    Case {
        name: "a struct of two integers",
        why: "nesting is where the buffer counting is easiest to get wrong in either direction",
        expected: None,
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("p", DataType::Struct(children), false)]),
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
                i64s(&[1, 2]),
                Vec::new(),
                i64s(&[3, 4]),
            ],
        },
    }
}

fn offset_one_past_the_end() -> Case {
    Case {
        name: "an offset one past the end of its buffer",
        why: "the classic off by one. The offsets are ordered and the buffer is nearly long \
              enough, so nothing about the array looks wrong until something reads the last value",
        expected: Some(Invariant::OffsetRange),
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("s", DataType::Utf8, false)]),
            rows: 2,
            nodes: vec![Node {
                length: 2,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), i32s(&[0, 2, 6]), b"hoyea".to_vec()],
        },
    }
}

fn null_count_off_by_one() -> Case {
    Case {
        name: "a null count off by one",
        why: "the one number in a batch that nothing else would catch. An array that lies about \
              its nulls produces wrong answers rather than an error",
        expected: Some(Invariant::NullCount),
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("a", DataType::Int64, true)]),
            rows: 3,
            nodes: vec![Node {
                length: 3,
                null_count: 2,
            }],
            // Every slot present, which is not two nulls.
            buffers: vec![vec![0b0000_0111], i64s(&[7, 8, 9])],
        },
    }
}

fn dictionary_index_equal_to_the_dictionary_length() -> Case {
    Case {
        name: "a dictionary index equal to the dictionary length",
        why: "in range for the arithmetic and one past the end of the data, which is what an off \
              by one in a decoder produces",
        expected: Some(Invariant::DictionaryIndex),
        subject: Subject::Dictionary {
            keys: i32s(&[0, 1, 3]),
            key_type: DataType::Int32,
            len: 3,
            dictionary_len: 3,
        },
    }
}

fn view_buffer_index_equal_to_the_buffer_count() -> Case {
    let mut views = Vec::with_capacity(16);
    views.extend_from_slice(&18u32.to_le_bytes());
    views.extend_from_slice(&[0u8; 4]);
    views.extend_from_slice(&1u32.to_le_bytes());
    views.extend_from_slice(&0u32.to_le_bytes());

    Case {
        name: "a view buffer index equal to the buffer count",
        why: "the same off by one as the dictionary key, in the one array layout where the number \
              of buffers is not fixed by the schema",
        expected: Some(Invariant::ViewBuffer),
        subject: Subject::Views {
            views,
            data: vec![b"hello there friend".to_vec()],
            len: 1,
        },
    }
}

fn length_times_width_that_overflows() -> Case {
    Case {
        name: "a length times an element width that overflows",
        why: "the arithmetic a checker does is itself an attack surface. A length that wraps when \
              multiplied by a width turns a bounds check into a permission slip",
        expected: Some(Invariant::Size),
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("a", DataType::Int64, false)]),
            rows: u64::MAX,
            nodes: vec![Node {
                length: u64::MAX,
                null_count: 0,
            }],
            buffers: vec![Vec::new(), i64s(&[1])],
        },
    }
}

fn child_one_row_short_of_its_parent() -> Case {
    let children = Fields::from(vec![Field::new("x", DataType::Int64, false)]);
    Case {
        name: "a child array one row short of its parent",
        why: "a struct's fields are read by the parent's length, so a short child is read past its \
              end on the last row and nowhere else",
        expected: Some(Invariant::ChildLength),
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("p", DataType::Struct(children), false)]),
            rows: 3,
            nodes: vec![
                Node {
                    length: 3,
                    null_count: 0,
                },
                Node {
                    length: 2,
                    null_count: 0,
                },
            ],
            buffers: vec![Vec::new(), Vec::new(), i64s(&[1, 2])],
        },
    }
}

fn schema_nesting_without_a_bound() -> Case {
    let mut data_type = DataType::Int64;
    for _ in 0..MAX_DEPTH + 10 {
        data_type = DataType::List(std::sync::Arc::new(Field::new("item", data_type, false)));
    }

    Case {
        name: "a schema nested deeper than anything will walk",
        why: "everything downstream of the guard walks a schema recursively, so an unbounded \
              schema is a stack overflow rather than an error, and a stack overflow is not \
              something a host can turn into a failed query",
        expected: Some(Invariant::Depth),
        subject: Subject::Batch {
            schema: Schema::new(vec![Field::new("deep", data_type, false)]),
            rows: 0,
            nodes: Vec::new(),
            buffers: Vec::<Vec<u8>>::new(),
        },
    }
}
