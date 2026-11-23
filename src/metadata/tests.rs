//! Tests of opening a file and extracting column-chunk facts.
//!
//! Footers are built from trees of Thrift values with the shared test encoder, so a
//! test can leave a field out, repeat it, give it the wrong type, or reorder fields.
//! The real files of parquet-testing are compared with arrow-rs.

use std::fs;
use std::path::Path;

use bench_support::corpus;
use parquet::basic::Type as ArrowType;
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData, ParquetMetaDataReader};
use parquet::file::statistics::Statistics as ArrowStatistics;
use proptest::prelude::*;

use super::template::Templates;
use super::{
    ChunkFacts, ChunkSlot, ColumnOrder, Columns, CompressionCodec, Encoding, ExtractError,
    FooterError, Location, LogicalType, Metadata, OpenError, Opened, PhysicalType, Repetition,
    Request, Schema, SchemaProblem, Statistics, Structure, TimeUnit, Unsupported, open,
};
use crate::Limits;
use crate::thrift::test_support::{Value, encoded};
use crate::thrift::{CompactReader, ElementType, FieldCursor, ThriftError};

// ---------------------------------------------------------------------------
// Building footers
// ---------------------------------------------------------------------------

/// The size given for the file a built footer belongs to. Chunk offsets in built
/// footers are far below it.
const FILE_SIZE: u64 = 1 << 30;

fn text(value: &str) -> Value {
    Value::Binary(value.as_bytes().to_vec())
}

fn structs(items: Vec<Value>) -> Value {
    Value::List(ElementType::Struct, items)
}

/// A schema element: its name and whatever other fields the test gives, in id order.
fn element(name: &str, mut fields: Vec<(i16, Value)>) -> Value {
    fields.push((4, text(name)));
    fields.sort_by_key(|(id, _)| *id);
    Value::Struct(fields)
}

/// The root of a schema with `children` children.
fn root(children: i32) -> Value {
    element("schema", vec![(5, Value::I32(children))])
}

/// A leaf column of the given physical type and repetition codes.
fn leaf(name: &str, physical_type: i32, repetition: i32) -> Value {
    element(
        name,
        vec![(1, Value::I32(physical_type)), (3, Value::I32(repetition))],
    )
}

/// A group with `children` children and the given repetition code.
fn group(name: &str, children: i32, repetition: i32) -> Value {
    element(
        name,
        vec![(3, Value::I32(repetition)), (5, Value::I32(children))],
    )
}

/// The required fields of a `ColumnMetaData` for a chunk at `offset`, `size` bytes long.
fn column_metadata(offset: i64, size: i64) -> Vec<(i16, Value)> {
    vec![
        (1, Value::I32(1)),
        (
            2,
            Value::List(ElementType::I32, vec![Value::I32(0), Value::I32(3)]),
        ),
        (3, Value::List(ElementType::Binary, vec![text("a")])),
        (4, Value::I32(6)),
        (5, Value::I64(100)),
        (6, Value::I64(size.saturating_mul(2))),
        (7, Value::I64(size)),
        (9, Value::I64(offset)),
    ]
}

/// A `ColumnChunk` holding the given `ColumnMetaData` fields.
fn chunk(metadata: Vec<(i16, Value)>) -> Value {
    Value::Struct(vec![(2, Value::I64(0)), (3, Value::Struct(metadata))])
}

/// A `RowGroup` of `rows` rows with the given column chunks.
fn row_group(chunks: Vec<Value>, rows: i64) -> Value {
    Value::Struct(vec![
        (1, structs(chunks)),
        (2, Value::I64(1000)),
        (3, Value::I64(rows)),
    ])
}

/// The fields of a footer with two `INT32` columns, the second optional, and two row
/// groups. Tests take this and change one thing.
fn simple_fields() -> Vec<(i16, Value)> {
    let schema = vec![root(2), leaf("a", 1, 0), leaf("b", 1, 1)];
    let row_groups = vec![
        row_group(
            vec![
                chunk(column_metadata(4, 100)),
                chunk(column_metadata(104, 50)),
            ],
            10,
        ),
        row_group(
            vec![
                chunk(column_metadata(154, 100)),
                chunk(column_metadata(254, 60)),
            ],
            7,
        ),
    ];
    vec![
        (1, Value::I32(2)),
        (2, structs(schema)),
        (3, Value::I64(17)),
        (4, structs(row_groups)),
        (6, text("parkay tests")),
    ]
}

fn footer_bytes(fields: Vec<(i16, Value)>) -> Vec<u8> {
    encoded(&Value::Struct(fields))
}

/// The end of a file whose footer is `footer`: the footer, its length, and the magic.
fn tail_of(footer: &[u8]) -> Vec<u8> {
    let mut tail = footer.to_vec();
    tail.extend_from_slice(&u32::try_from(footer.len()).unwrap().to_le_bytes());
    tail.extend_from_slice(b"PAR1");
    tail
}

fn open_footer(footer: &[u8]) -> Result<Opened, OpenError> {
    open(&tail_of(footer), FILE_SIZE, &Limits::new(), None)
}

fn open_fields(fields: Vec<(i16, Value)>) -> Result<Opened, OpenError> {
    open_footer(&footer_bytes(fields))
}

/// Opens a footer that must open, and returns its metadata and schema.
fn opened(fields: Vec<(i16, Value)>) -> (Metadata, Schema) {
    match open_fields(fields).unwrap() {
        Opened::File { metadata, schema } => (metadata, schema.unwrap()),
        Opened::MissingBytes { .. } => panic!("the tail holds the whole footer"),
    }
}

/// The error inside the footer that opening these fields fails with.
fn footer_error(fields: Vec<(i16, Value)>) -> FooterError {
    match open_fields(fields) {
        Err(OpenError::Footer { source, .. }) => source,
        other => panic!("expected a footer error, got {other:?}"),
    }
}

/// Replaces the value of field `id`.
fn with_field(mut fields: Vec<(i16, Value)>, id: i16, value: Value) -> Vec<(i16, Value)> {
    let field = fields.iter_mut().find(|(field, _)| *field == id).unwrap();
    field.1 = value;
    fields
}

fn without_field(mut fields: Vec<(i16, Value)>, id: i16) -> Vec<(i16, Value)> {
    fields.retain(|(field, _)| *field != id);
    fields
}

fn slot(row_group: u32, column: u32) -> ChunkSlot {
    ChunkSlot { row_group, column }
}

fn everything(metadata: &Metadata) -> Request<'static> {
    Request::new(Columns::All, 0..metadata.row_group_count())
        .with_location()
        .with_statistics()
}

// ---------------------------------------------------------------------------
// The end of the file
// ---------------------------------------------------------------------------

#[test]
fn tail_shorter_than_the_trailer_asks_for_the_last_eight_bytes() {
    let tail = tail_of(&footer_bytes(simple_fields()));
    for supplied in [0, 1, 7] {
        let short = &tail[tail.len() - supplied..];
        let Opened::MissingBytes { start, end } =
            open(short, FILE_SIZE, &Limits::new(), None).unwrap()
        else {
            panic!("a tail of {supplied} bytes cannot hold the trailer");
        };
        assert_eq!(start, FILE_SIZE - 8);
        assert_eq!(end, FILE_SIZE - supplied as u64);
    }
}

#[test]
fn tail_without_the_whole_footer_asks_for_exactly_the_missing_bytes() {
    let footer = footer_bytes(simple_fields());
    let tail = tail_of(&footer);
    for supplied in [8, 9, tail.len() - 1] {
        let short = &tail[tail.len() - supplied..];
        let Opened::MissingBytes { start, end } =
            open(short, FILE_SIZE, &Limits::new(), None).unwrap()
        else {
            panic!("a tail of {supplied} bytes does not hold the footer");
        };
        assert_eq!(start, FILE_SIZE - tail.len() as u64);
        assert_eq!(end, FILE_SIZE - supplied as u64);
    }
}

#[test]
fn tail_longer_than_the_footer_is_accepted() {
    let mut tail = vec![0xAB; 1000];
    tail.extend_from_slice(&tail_of(&footer_bytes(simple_fields())));
    let Opened::File { metadata, .. } = open(&tail, FILE_SIZE, &Limits::new(), None).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    assert_eq!(metadata.row_count(), 17);
}

#[test]
fn tail_and_file_size_that_disagree_are_an_error() {
    let tail = tail_of(&footer_bytes(simple_fields()));
    let tail_size = tail.len() as u64;
    assert_eq!(
        open(&tail, tail_size - 1, &Limits::new(), None).unwrap_err(),
        OpenError::TailLongerThanFile {
            tail_size,
            file_size: tail_size - 1
        }
    );
    assert_eq!(
        open(&[], 7, &Limits::new(), None).unwrap_err(),
        OpenError::FileTooSmall { file_size: 7 }
    );
}

#[test]
fn wrong_magic_is_not_parquet_and_an_encrypted_footer_is_unsupported() {
    let mut tail = tail_of(&footer_bytes(simple_fields()));
    let magic = tail.len() - 4;
    tail[magic..].copy_from_slice(b"PARE");
    assert_eq!(
        open(&tail, FILE_SIZE, &Limits::new(), None).unwrap_err(),
        OpenError::EncryptedFooter
    );
    tail[magic..].copy_from_slice(b"PAR2");
    assert_eq!(
        open(&tail, FILE_SIZE, &Limits::new(), None).unwrap_err(),
        OpenError::NotParquet { found: *b"PAR2" }
    );
}

#[test]
fn footer_size_is_checked_against_the_limit_and_the_file() {
    let tail = tail_of(&footer_bytes(simple_fields()));
    let footer_size = u32::try_from(tail.len() - 8).unwrap();
    let limits = Limits::new().with_footer_size_max(footer_size - 1);
    assert_eq!(
        open(&tail, FILE_SIZE, &limits, None).unwrap_err(),
        OpenError::FooterTooLarge {
            footer_size,
            limit: footer_size - 1
        }
    );
    // A trailer alone, claiming a footer the file has no room for.
    let file_size = u64::from(footer_size) + 7;
    assert_eq!(
        open(&tail[tail.len() - 8..], file_size, &Limits::new(), None).unwrap_err(),
        OpenError::FooterLongerThanFile {
            footer_size,
            file_size
        }
    );
}

#[test]
fn empty_footer_is_a_thrift_error_at_the_footer_offset() {
    let error = open(&tail_of(&[]), FILE_SIZE, &Limits::new(), None).unwrap_err();
    let OpenError::Footer {
        footer_offset,
        source,
    } = error
    else {
        panic!("expected a footer error, got {error:?}");
    };
    assert_eq!(footer_offset, FILE_SIZE - 8);
    assert!(matches!(
        source,
        FooterError::Thrift(ThriftError::UnexpectedEof { offset: 0, .. })
    ));
}

// ---------------------------------------------------------------------------
// The footer as a whole
// ---------------------------------------------------------------------------

#[test]
fn file_level_facts_are_read() {
    let mut fields = simple_fields();
    fields.push((
        7,
        structs(vec![
            Value::Struct(vec![(1, Value::Struct(vec![]))]),
            Value::Struct(vec![(2, Value::Struct(vec![]))]),
        ]),
    ));
    let (metadata, schema) = opened(fields);
    assert_eq!(
        (
            metadata.version(),
            metadata.row_count(),
            metadata.file_size()
        ),
        (2, 17, FILE_SIZE)
    );
    assert_eq!(
        (
            metadata.row_group_count(),
            metadata.column_count(),
            schema.column_count()
        ),
        (2, 2, 2)
    );
    let row_counts = [0, 1, 2].map(|row_group| metadata.row_group_row_count(row_group));
    assert_eq!(row_counts, [Some(10), Some(7), None]);
    assert_eq!(metadata.row_group_byte_size(1), Some(1000));
    assert_eq!(metadata.created_by(), Ok(Some("parkay tests")));
    let column_orders: Vec<_> = metadata.column_orders().unwrap().unwrap().collect();
    let expected = [Ok(ColumnOrder::TypeDefined), Ok(ColumnOrder::Ieee754Total)];
    assert_eq!(column_orders, expected);
}

#[test]
fn key_value_pairs_are_read_in_order() {
    let mut fields = simple_fields();
    fields.insert(
        4,
        (
            5,
            structs(vec![
                Value::Struct(vec![(1, text("k1")), (2, text("v1"))]),
                Value::Struct(vec![(1, text("k2"))]),
            ]),
        ),
    );
    let (metadata, _) = opened(fields);
    let pairs: Vec<_> = metadata
        .key_value_metadata()
        .map(|pair| pair.unwrap())
        .map(|pair| (pair.key(), pair.value()))
        .collect();
    assert_eq!(pairs, [("k1", Some("v1")), ("k2", None)]);
}

#[test]
fn a_file_without_optional_facts_has_none() {
    let (metadata, _) = opened(without_field(simple_fields(), 6));
    assert_eq!(metadata.created_by(), Ok(None));
    assert!(metadata.column_orders().unwrap().is_none());
    assert_eq!(metadata.key_value_metadata().count(), 0);
}

#[test]
fn each_required_field_is_required() {
    for field in [1, 2, 3, 4] {
        assert_eq!(
            footer_error(without_field(simple_fields(), i16::from(field))),
            FooterError::MissingField {
                structure: Structure::FileMetaData,
                field
            }
        );
    }
}

#[test]
fn a_repeated_field_is_an_error() {
    let mut fields = simple_fields();
    fields.push((3, Value::I64(17)));
    assert!(matches!(
        footer_error(fields),
        FooterError::DuplicateField {
            structure: Structure::FileMetaData,
            field: 3,
            ..
        }
    ));
}

#[test]
fn a_field_with_the_wrong_wire_type_is_skipped_as_unknown() {
    // The row count as an `i32`: skipped, so the required field is then missing.
    let fields = with_field(simple_fields(), 3, Value::I32(17));
    assert_eq!(
        footer_error(fields),
        FooterError::MissingField {
            structure: Structure::FileMetaData,
            field: 3
        }
    );
    // An optional field with the wrong type is simply absent.
    let (metadata, _) = opened(with_field(simple_fields(), 6, Value::I32(1)));
    assert_eq!(metadata.created_by(), Ok(None));
}

#[test]
fn unknown_fields_are_skipped_and_field_order_does_not_matter() {
    let mut fields = simple_fields();
    fields.push((40, Value::Struct(vec![(1, Value::I64(5))])));
    fields.push((300, text("far beyond the short form")));
    // Reversed, every field after the first takes the long form of its id, and the row
    // groups come before the schema.
    fields.reverse();
    let (metadata, schema) = opened(fields);
    assert_eq!(metadata.row_count(), 17);
    assert_eq!(metadata.row_group_count(), 2);
    assert_eq!(schema.column_count(), 2);
    let mut facts = ChunkFacts::new();
    metadata
        .extract(&everything(&metadata), &mut facts)
        .unwrap();
    assert_eq!(facts.location(slot(1, 1)).unwrap().start(), 254);
}

#[test]
fn encryption_is_unsupported() {
    for (id, value) in [(8, Value::Struct(vec![])), (9, text("key"))] {
        let mut fields = simple_fields();
        fields.push((id, value));
        assert!(matches!(
            footer_error(fields),
            FooterError::Unsupported {
                feature: Unsupported::Encryption,
                ..
            }
        ));
    }
}

#[test]
fn bytes_after_the_stop_marker_are_ignored() {
    let mut footer = footer_bytes(simple_fields());
    footer.extend_from_slice(&[0xFF; 28]);
    let Opened::File { metadata, .. } = open_footer(&footer).unwrap() else {
        panic!("the tail holds the whole footer");
    };
    assert_eq!(metadata.row_count(), 17);
}

#[test]
fn a_row_group_must_have_one_chunk_per_column() {
    let mut row_groups = vec![
        row_group(
            vec![chunk(column_metadata(4, 1)), chunk(column_metadata(5, 1))],
            1,
        ),
        row_group(vec![chunk(column_metadata(6, 1))], 1),
    ];
    let fields = with_field(simple_fields(), 4, structs(row_groups.clone()));
    assert_eq!(
        footer_error(fields),
        FooterError::ColumnCount {
            row_group: 1,
            found: 1,
            expected: 2
        }
    );
    // With the row groups before the schema, they are checked against each other and
    // then against the schema.
    row_groups.truncate(1);
    row_groups[0] = row_group(vec![chunk(column_metadata(4, 1))], 1);
    let mut row_groups_first = with_field(simple_fields(), 4, structs(row_groups));
    row_groups_first.swap(1, 3);
    assert_eq!(
        footer_error(row_groups_first),
        FooterError::ColumnCount {
            row_group: 0,
            found: 1,
            expected: 2
        }
    );
}

#[test]
fn column_orders_must_number_the_columns() {
    let mut fields = simple_fields();
    fields.push((7, structs(vec![Value::Struct(vec![])])));
    // Opening does not look at the orders. Asking for them does.
    let (metadata, _) = opened(fields);
    assert_eq!(
        metadata.column_orders().unwrap_err(),
        FooterError::ColumnOrderCount {
            found: 1,
            expected: 2
        }
    );
}

#[test]
fn a_writer_name_that_is_not_utf8_is_reported_when_it_is_read() {
    let fields = with_field(simple_fields(), 6, Value::Binary(vec![0xFF, 0xFE]));
    let (metadata, _) = opened(fields);
    assert!(matches!(
        metadata.created_by(),
        Err(FooterError::InvalidUtf8 {
            structure: Structure::FileMetaData,
            field: 6,
            ..
        })
    ));
}

#[test]
fn an_order_this_reader_does_not_know_is_unrecognized() {
    let mut fields = simple_fields();
    fields.push((
        7,
        structs(vec![
            Value::Struct(vec![(9, Value::Struct(vec![]))]),
            Value::Struct(vec![(3, Value::Struct(vec![]))]),
        ]),
    ));
    let (metadata, _) = opened(fields);
    let column_orders: Vec<_> = metadata.column_orders().unwrap().unwrap().collect();
    let expected = [
        Ok(ColumnOrder::Unrecognized),
        Ok(ColumnOrder::Int96Timestamp),
    ];
    assert_eq!(column_orders, expected);
}

#[test]
fn negative_counts_are_invalid() {
    assert!(matches!(
        footer_error(with_field(simple_fields(), 3, Value::I64(-1))),
        FooterError::InvalidValue {
            structure: Structure::FileMetaData,
            field: 3,
            ..
        }
    ));
    let chunks = vec![chunk(column_metadata(4, 1)), chunk(column_metadata(5, 1))];
    let row_groups = structs(vec![Value::Struct(vec![
        (1, structs(chunks)),
        (2, Value::I64(1)),
        (3, Value::I64(-5)),
    ])]);
    assert!(matches!(
        footer_error(with_field(simple_fields(), 4, row_groups)),
        FooterError::InvalidValue {
            structure: Structure::RowGroup,
            field: 3,
            ..
        }
    ));
}

#[test]
fn a_row_group_needs_its_required_fields() {
    for field in [1, 2, 3] {
        let mut group_fields = vec![
            (
                1,
                structs(vec![
                    chunk(column_metadata(4, 1)),
                    chunk(column_metadata(5, 1)),
                ]),
            ),
            (2, Value::I64(1)),
            (3, Value::I64(1)),
        ];
        group_fields.retain(|(id, _)| *id != i16::from(field));
        let row_groups = structs(vec![Value::Struct(group_fields)]);
        assert_eq!(
            footer_error(with_field(simple_fields(), 4, row_groups)),
            FooterError::MissingField {
                structure: Structure::RowGroup,
                field
            }
        );
    }
}

#[test]
fn a_malformed_key_value_pair_is_reported_when_it_is_read() {
    let mut fields = simple_fields();
    fields.insert(
        4,
        (
            5,
            structs(vec![
                Value::Struct(vec![(1, text("good"))]),
                Value::Struct(vec![(2, text("a value with no key"))]),
                Value::Struct(vec![(1, text("never reached"))]),
            ]),
        ),
    );
    let (metadata, _) = opened(fields);
    let pairs: Vec<_> = metadata.key_value_metadata().collect();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].unwrap().key(), "good");
    assert_eq!(
        pairs[1],
        Err(FooterError::MissingField {
            structure: Structure::KeyValue,
            field: 1
        })
    );
}

// ---------------------------------------------------------------------------
// The schema
// ---------------------------------------------------------------------------

/// Opens a footer with the given schema elements and no row groups.
fn schema_of(elements: Vec<Value>) -> Result<Schema, FooterError> {
    let fields = with_field(simple_fields(), 2, structs(elements));
    let fields = with_field(fields, 4, structs(vec![]));
    match open_fields(fields) {
        Ok(Opened::File { schema, .. }) => Ok(schema.unwrap()),
        Ok(Opened::MissingBytes { .. }) => panic!("the tail holds the whole footer"),
        Err(OpenError::Footer { source, .. }) => Err(source),
        Err(other) => panic!("expected a footer error, got {other:?}"),
    }
}

/// A schema with a required column, a list of optional strings, and a repeated column:
///
/// ```text
/// message schema {
///   required int32 id;
///   optional group tags (LIST) { repeated group list { optional binary element; } }
///   repeated int64 hits;
/// }
/// ```
fn nested_schema() -> Schema {
    schema_of(vec![
        root(3),
        leaf("id", 1, 0),
        group("tags", 1, 1),
        group("list", 1, 2),
        leaf("element", 6, 1),
        leaf("hits", 2, 2),
    ])
    .unwrap()
}

#[test]
fn a_nested_schema_has_a_root_and_leaf_columns() {
    let schema = nested_schema();
    assert_eq!((schema.node_count(), schema.column_count()), (6, 3));
    assert!(schema.node(6).is_none());
    assert!(schema.column(3).is_none());
    let root = schema.node(0).unwrap();
    assert_eq!((root.name(), root.child_count()), ("schema", 3));
    assert!(root.parent().is_none());
    assert_eq!(root.path().len(), 0);
    assert_eq!(root.physical_type(), None);
    let columns: Vec<_> = (0..3)
        .map(|column| schema.column(column).unwrap())
        .map(|column| {
            (
                column.index(),
                column.path(),
                column.physical_type().unwrap(),
            )
        })
        .collect();
    let expected = [
        (1, vec!["id"], PhysicalType::Int32),
        (4, vec!["tags", "list", "element"], PhysicalType::ByteArray),
        (5, vec!["hits"], PhysicalType::Int64),
    ];
    assert_eq!(columns, expected);
    assert_eq!(schema.column(1).unwrap().parent().unwrap().name(), "list");
}

#[test]
fn a_nested_schema_has_levels() {
    let schema = nested_schema();
    let levels: Vec<_> = (0..3)
        .map(|column| schema.column(column).unwrap())
        .map(|column| {
            (
                column.repetition().unwrap(),
                column.definition_level_max(),
                column.repetition_level_max(),
            )
        })
        .collect();
    let expected = [
        (Repetition::Required, 0, 0),
        (Repetition::Optional, 3, 1),
        (Repetition::Repeated, 1, 1),
    ];
    assert_eq!(levels, expected);
}

#[test]
fn logical_types_are_decoded() {
    let logical = |member: i16, fields: Vec<(i16, Value)>| {
        Value::Struct(vec![(member, Value::Struct(fields))])
    };
    let unit = |member: i16| Value::Struct(vec![(member, Value::Struct(vec![]))]);
    let cases = vec![
        (logical(1, vec![]), LogicalType::String),
        (logical(6, vec![]), LogicalType::Date),
        (logical(14, vec![]), LogicalType::Uuid),
        (logical(16, vec![(1, Value::I8(1))]), LogicalType::Variant),
        (logical(17, vec![(1, text("crs"))]), LogicalType::Geometry),
        (logical(99, vec![]), LogicalType::Unrecognized),
        (
            logical(5, vec![(1, Value::I32(2)), (2, Value::I32(9))]),
            LogicalType::Decimal {
                scale: 2,
                precision: 9,
            },
        ),
        (
            logical(7, vec![(1, Value::Bool(false)), (2, unit(2))]),
            LogicalType::Time {
                adjusted_to_utc: false,
                unit: TimeUnit::Microseconds,
            },
        ),
        (
            logical(8, vec![(1, Value::Bool(true)), (2, unit(3))]),
            LogicalType::Timestamp {
                adjusted_to_utc: true,
                unit: TimeUnit::Nanoseconds,
            },
        ),
        (
            logical(10, vec![(1, Value::I8(16)), (2, Value::Bool(false))]),
            LogicalType::Integer {
                bit_width: 16,
                signed: false,
            },
        ),
    ];
    for (value, expected) in cases {
        let column = element(
            "c",
            vec![(1, Value::I32(2)), (3, Value::I32(0)), (10, value)],
        );
        let schema = schema_of(vec![root(1), column]).unwrap();
        assert_eq!(schema.column(0).unwrap().logical_type(), Some(expected));
    }
}

#[test]
fn an_older_converted_type_becomes_the_equivalent_logical_type() {
    let converted = |code: i32, extra: Vec<(i16, Value)>| {
        let mut fields = vec![
            (1, Value::I32(2)),
            (3, Value::I32(0)),
            (6, Value::I32(code)),
        ];
        fields.extend(extra);
        let schema = schema_of(vec![root(1), element("c", fields)]).unwrap();
        schema.column(0).unwrap().logical_type()
    };
    assert_eq!(converted(0, vec![]), Some(LogicalType::String));
    assert_eq!(converted(21, vec![]), Some(LogicalType::Interval));
    assert_eq!(
        converted(5, vec![(7, Value::I32(3)), (8, Value::I32(12))]),
        Some(LogicalType::Decimal {
            scale: 3,
            precision: 12
        })
    );
    assert_eq!(
        converted(10, vec![]),
        Some(LogicalType::Timestamp {
            adjusted_to_utc: true,
            unit: TimeUnit::Microseconds
        })
    );
    assert_eq!(
        converted(12, vec![]),
        Some(LogicalType::Integer {
            bit_width: 16,
            signed: false
        })
    );
    // The newer annotation wins when a writer sets both.
    let newer = Value::Struct(vec![(12, Value::Struct(vec![]))]);
    assert_eq!(converted(0, vec![(10, newer)]), Some(LogicalType::Json));
}

#[test]
fn field_ids_and_type_lengths_are_kept() {
    let column = element(
        "c",
        vec![
            (1, Value::I32(7)),
            (2, Value::I32(16)),
            (3, Value::I32(0)),
            (9, Value::I32(42)),
        ],
    );
    let schema = schema_of(vec![root(1), column]).unwrap();
    let decoded = schema.column(0).unwrap();
    assert_eq!(
        decoded.physical_type(),
        Some(PhysicalType::FixedLenByteArray)
    );
    assert_eq!(decoded.type_length(), Some(16));
    assert_eq!(decoded.field_id(), Some(42));
}

#[test]
fn a_schema_that_is_not_one_tree_is_rejected() {
    let problem = |elements, problem, element| {
        assert_eq!(
            schema_of(elements),
            Err(FooterError::Schema { problem, element })
        );
    };
    problem(vec![], SchemaProblem::Empty, 0);
    problem(
        vec![root(2), leaf("a", 1, 0)],
        SchemaProblem::ChildrenMissing,
        2,
    );
    problem(
        vec![root(1), leaf("a", 1, 0), leaf("b", 1, 0)],
        SchemaProblem::ExtraRoot,
        2,
    );
    problem(vec![root(0), leaf("a", 1, 0)], SchemaProblem::ExtraRoot, 1);
    let no_repetition = element("a", vec![(1, Value::I32(1))]);
    problem(
        vec![root(1), no_repetition],
        SchemaProblem::RepetitionMissing,
        1,
    );
    // A group needs one too, and a repetition with the wrong wire type is skipped as
    // unknown, so it counts as missing instead of changing the levels below it.
    let group_without = element("g", vec![(5, Value::I32(1))]);
    problem(
        vec![root(1), group_without, leaf("a", 1, 0)],
        SchemaProblem::RepetitionMissing,
        1,
    );
    let mistyped = element("g", vec![(3, Value::I64(1)), (5, Value::I32(1))]);
    problem(
        vec![root(1), mistyped, leaf("a", 1, 0)],
        SchemaProblem::RepetitionMissing,
        1,
    );
    problem(
        vec![root(1), leaf("a", 7, 0)],
        SchemaProblem::LengthMissing,
        1,
    );
}

#[test]
fn an_empty_schema_and_an_empty_group_have_no_columns() {
    let schema = schema_of(vec![root(0)]).unwrap();
    assert_eq!(schema.column_count(), 0);
    // A field with neither a type nor children is a group with nothing in it.
    let empty_group = element("g", vec![(3, Value::I32(1))]);
    let with_group = schema_of(vec![root(2), empty_group, leaf("a", 1, 0)]).unwrap();
    assert_eq!(with_group.column_count(), 1);
    assert_eq!(with_group.column(0).unwrap().index(), 2);
}

#[test]
fn invalid_schema_values_are_rejected() {
    let invalid = |fields: Vec<(i16, Value)>, field: u8| {
        assert!(matches!(
            schema_of(vec![root(1), element("c", fields)]),
            Err(FooterError::InvalidValue { structure: Structure::SchemaElement, field: found, .. })
                if found == field
        ));
    };
    invalid(vec![(1, Value::I32(8)), (3, Value::I32(0))], 1);
    invalid(vec![(1, Value::I32(1)), (3, Value::I32(3))], 3);
    invalid(
        vec![(1, Value::I32(1)), (3, Value::I32(0)), (5, Value::I32(-1))],
        5,
    );
    invalid(
        vec![(1, Value::I32(1)), (3, Value::I32(0)), (6, Value::I32(22))],
        6,
    );
    // A logical type with no member, and one with two.
    let none = Value::Struct(vec![]);
    let two = Value::Struct(vec![(1, Value::Struct(vec![])), (6, Value::Struct(vec![]))]);
    for logical_type in [none, two] {
        invalid(
            vec![(1, Value::I32(1)), (3, Value::I32(0)), (10, logical_type)],
            10,
        );
    }
}

#[test]
fn a_name_that_is_not_utf8_is_rejected() {
    let column = Value::Struct(vec![
        (1, Value::I32(1)),
        (3, Value::I32(0)),
        (4, Value::Binary(vec![0xFF, 0xFE])),
    ]);
    assert!(matches!(
        schema_of(vec![root(1), column]),
        Err(FooterError::InvalidUtf8 {
            structure: Structure::SchemaElement,
            field: 4,
            ..
        })
    ));
}

#[test]
fn a_schema_deeper_than_the_limit_is_rejected() {
    // A chain of nested groups ending in one leaf: the leaf is at depth `groups + 1`.
    let chain = |groups: usize| {
        let mut elements = vec![root(1)];
        elements.extend((0..groups).map(|_| group("g", 1, 1)));
        elements.push(leaf("leaf", 1, 1));
        footer_bytes(with_field(
            with_field(simple_fields(), 2, structs(elements)),
            4,
            structs(vec![]),
        ))
    };
    let limits = Limits::new().with_schema_depth_max(4);
    let Opened::File { schema, .. } = open(&tail_of(&chain(3)), FILE_SIZE, &limits, None).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    let schema = schema.unwrap();
    let leaf = schema.column(0).unwrap();
    assert_eq!(leaf.definition_level_max(), 4);
    assert_eq!(leaf.path().len(), 4);
    assert_eq!(
        open(&tail_of(&chain(4)), FILE_SIZE, &limits, None).unwrap_err(),
        OpenError::Footer {
            footer_offset: FILE_SIZE - 8 - chain(4).len() as u64,
            source: FooterError::SchemaTooDeep {
                element: 5,
                limit: 4
            }
        }
    );
    // The deepest schema the limits allow at all keeps its levels within a byte.
    let deepest = Limits::new().with_schema_depth_max(u8::MAX);
    let Opened::File {
        schema: deepest_schema,
        ..
    } = open(&tail_of(&chain(254)), FILE_SIZE, &deepest, None).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    let deepest_leaf = deepest_schema.unwrap();
    assert_eq!(deepest_leaf.column(0).unwrap().definition_level_max(), 255);
}

#[test]
fn a_shared_schema_is_recognized_by_its_bytes() {
    let (first, schema) = opened(simple_fields());
    // A second file of the same dataset: the same schema, other row groups.
    let other_rows = with_field(simple_fields(), 3, Value::I64(99));
    let other_rows = with_field(other_rows, 4, structs(vec![]));
    let tail = tail_of(&footer_bytes(other_rows));
    let Opened::File {
        metadata,
        schema: decoded,
    } = open(&tail, FILE_SIZE, &Limits::new(), Some(&schema)).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    assert!(
        decoded.is_none(),
        "the supplied schema is this file's schema"
    );
    assert_eq!(metadata.version(), first.version());
    assert_eq!(
        (
            metadata.column_count(),
            metadata.row_count(),
            metadata.row_group_count()
        ),
        (2, 99, 0)
    );
}

#[test]
fn a_file_with_another_schema_is_decoded_as_usual() {
    let (_, schema) = opened(simple_fields());
    let renamed = with_field(
        simple_fields(),
        2,
        structs(vec![root(2), leaf("a", 1, 0), leaf("z", 1, 1)]),
    );
    let tail = tail_of(&footer_bytes(renamed));
    let Opened::File {
        schema: decoded, ..
    } = open(&tail, FILE_SIZE, &Limits::new(), Some(&schema)).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    assert_eq!(decoded.unwrap().column(1).unwrap().name(), "z");
}

#[test]
fn a_shared_schema_still_checks_the_rest_of_the_footer() {
    let (_, schema) = opened(simple_fields());
    let open_shared = |fields: Vec<(i16, Value)>| {
        let tail = tail_of(&footer_bytes(fields));
        match open(&tail, FILE_SIZE, &Limits::new(), Some(&schema)) {
            Err(OpenError::Footer { source, .. }) => source,
            other => panic!("expected a footer error, got {other:?}"),
        }
    };
    // The same leading bytes, then a second version field.
    let mut repeated = simple_fields();
    repeated.push((1, Value::I32(2)));
    assert!(matches!(
        open_shared(repeated),
        FooterError::DuplicateField {
            structure: Structure::FileMetaData,
            field: 1,
            ..
        }
    ));
    // The same leading bytes, but a row group with a chunk too many.
    let row_groups = structs(vec![row_group(
        vec![
            chunk(column_metadata(4, 1)),
            chunk(column_metadata(5, 1)),
            chunk(column_metadata(6, 1)),
        ],
        1,
    )]);
    assert_eq!(
        open_shared(with_field(simple_fields(), 4, row_groups)),
        FooterError::ColumnCount {
            row_group: 0,
            found: 3,
            expected: 2
        }
    );
}

#[test]
fn a_schema_that_does_not_follow_the_version_is_never_shared() {
    // The schema first and the version second: valid, but the leading bytes are not the
    // version and then the schema, so they are not recorded.
    let mut fields = simple_fields();
    fields.swap(0, 1);
    let (_, schema) = opened(fields.clone());
    let tail = tail_of(&footer_bytes(fields));
    let Opened::File {
        schema: decoded, ..
    } = open(&tail, FILE_SIZE, &Limits::new(), Some(&schema)).unwrap()
    else {
        panic!("the tail holds the whole footer");
    };
    assert_eq!(decoded.unwrap(), schema);
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// A footer with one column and one row group whose chunk has the given metadata.
fn one_chunk(metadata: Vec<(i16, Value)>) -> Metadata {
    one_chunk_struct(chunk(metadata))
}

fn one_chunk_struct(column_chunk: Value) -> Metadata {
    let fields = with_field(simple_fields(), 2, structs(vec![root(1), leaf("a", 1, 1)]));
    let fields = with_field(fields, 4, structs(vec![row_group(vec![column_chunk], 5)]));
    opened(fields).0
}

/// The error extracting everything from a file with one chunk fails with.
fn chunk_error(metadata: &Metadata, request: &Request<'_>) -> FooterError {
    match metadata.extract(request, &mut ChunkFacts::new()) {
        Err(ExtractError::ColumnChunk {
            row_group: 0,
            column: 0,
            source,
        }) => source,
        other => panic!("expected a column chunk error, got {other:?}"),
    }
}

fn statistics_fields() -> Vec<(i16, Value)> {
    vec![
        (1, Value::Binary(vec![9, 9])),
        (2, Value::Binary(vec![1, 1])),
        (3, Value::I64(4)),
        (4, Value::I64(3)),
        (5, Value::Binary(vec![7, 0, 0, 0])),
        (6, Value::Binary(vec![2, 0, 0, 0])),
        (7, Value::Bool(true)),
        (8, Value::Bool(false)),
        (9, Value::I64(0)),
    ]
}

#[test]
fn locations_are_extracted_for_every_chunk() {
    let (metadata, _) = opened(simple_fields());
    let mut facts = ChunkFacts::new();
    let request = Request::new(Columns::All, 0..2).with_location();
    metadata.extract(&request, &mut facts).unwrap();
    assert_eq!((facts.row_group_count(), facts.column_count()), (2, 2));
    let found: Vec<_> = [slot(0, 0), slot(0, 1), slot(1, 0), slot(1, 1)]
        .map(|slot| *facts.location(slot).unwrap())
        .map(|location| {
            (
                location.start(),
                location.data_page_offset(),
                location.compressed_size(),
                location.uncompressed_size(),
            )
        })
        .into();
    let expected = [
        (4, 4, 100, 200),
        (104, 104, 50, 100),
        (154, 154, 100, 200),
        (254, 254, 60, 120),
    ];
    assert_eq!(found, expected);
    let location = facts.location(slot(1, 1)).unwrap();
    assert_eq!(location.dictionary_page_offset(), None);
    assert_eq!(location.value_count(), 100);
    assert_eq!(location.codec(), CompressionCodec::Zstd);
    let encodings = [Encoding::Plain, Encoding::Rle, Encoding::RleDictionary]
        .map(|encoding| location.encodings().contains(encoding));
    assert_eq!(encodings, [true, true, false]);
}

#[test]
fn facts_outside_the_request_are_absent() {
    let (metadata, _) = opened(simple_fields());
    let mut facts = ChunkFacts::new();
    let request = Request::new(Columns::All, 0..2).with_location();
    metadata.extract(&request, &mut facts).unwrap();
    assert!(facts.location(slot(2, 0)).is_none());
    assert!(facts.location(slot(0, 2)).is_none());
    assert!(
        facts.statistics(slot(0, 0)).is_none(),
        "statistics were not requested"
    );
}

/// Opens a file with one chunk that has the given `Statistics` fields, and extracts them.
fn statistics_of(statistics_fields: Vec<(i16, Value)>) -> (Metadata, Statistics) {
    let mut fields = column_metadata(4, 100);
    fields.push((12, Value::Struct(statistics_fields)));
    let metadata = one_chunk(fields);
    let mut facts = ChunkFacts::new();
    metadata
        .extract(&everything(&metadata), &mut facts)
        .unwrap();
    let statistics = *facts.statistics(slot(0, 0)).unwrap();
    (metadata, statistics)
}

#[test]
fn statistics_are_extracted_with_their_bounds_in_the_footer() {
    let (metadata, statistics) = statistics_of(statistics_fields());
    let counts = (
        statistics.null_count(),
        statistics.distinct_count(),
        statistics.nan_count(),
    );
    assert_eq!(counts, (Some(4), Some(3), Some(0)));
    assert_eq!(statistics.min(&metadata), Some(&[2, 0, 0, 0][..]));
    assert_eq!(statistics.max(&metadata), Some(&[7, 0, 0, 0][..]));
    assert_eq!(statistics.min_is_exact(), Some(false));
    assert_eq!(statistics.max_is_exact(), Some(true));
    assert!(!statistics.bounds_are_deprecated());
}

#[test]
fn the_older_bounds_are_used_when_there_are_no_newer_ones() {
    let old_only = statistics_fields()
        .into_iter()
        .filter(|(id, _)| matches!(id, 1 | 2))
        .collect();
    let (metadata, statistics) = statistics_of(old_only);
    assert!(statistics.bounds_are_deprecated());
    assert_eq!(statistics.min(&metadata), Some(&[1, 1][..]));
    assert_eq!(statistics.max(&metadata), Some(&[9, 9][..]));
    assert_eq!(statistics.null_count(), None);
    assert_eq!(statistics.min_is_exact(), None);
}

#[test]
fn the_older_bounds_are_not_used_beside_a_newer_one() {
    let one_newer = statistics_fields()
        .into_iter()
        .filter(|(id, _)| matches!(id, 1 | 2 | 5))
        .collect();
    let (metadata, statistics) = statistics_of(one_newer);
    assert!(!statistics.bounds_are_deprecated());
    assert_eq!(statistics.min(&metadata), None);
    assert_eq!(statistics.max(&metadata), Some(&[7, 0, 0, 0][..]));
}

#[test]
fn a_chunk_without_statistics_has_empty_statistics() {
    let metadata = one_chunk(column_metadata(4, 100));
    let mut facts = ChunkFacts::new();
    metadata
        .extract(&everything(&metadata), &mut facts)
        .unwrap();
    let statistics = facts.statistics(slot(0, 0)).unwrap();
    assert_eq!(statistics.null_count(), None);
    assert_eq!(statistics.min(&metadata), None);
    assert_eq!(statistics.max(&metadata), None);
}

#[test]
fn the_chunk_starts_at_its_dictionary_page_when_it_has_one() {
    let start = |data: i64, dictionary: Option<i64>| {
        let mut fields = column_metadata(data, 100);
        if let Some(dictionary) = dictionary {
            fields.push((11, Value::I64(dictionary)));
        }
        let metadata = one_chunk(fields);
        let mut facts = ChunkFacts::new();
        metadata
            .extract(&everything(&metadata), &mut facts)
            .unwrap();
        let location = *facts.location(slot(0, 0)).unwrap();
        (location.start(), location.dictionary_page_offset())
    };
    assert_eq!(start(500, None), (500, None));
    assert_eq!(start(500, Some(400)), (400, Some(400)));
    // Older writers left a zero, or an offset after the data, on a chunk with no
    // dictionary page.
    assert_eq!(start(500, Some(0)), (500, Some(0)));
    assert_eq!(start(500, Some(600)), (500, Some(600)));
}

#[test]
fn only_the_requested_columns_and_row_groups_are_extracted() {
    let (metadata, _) = opened(simple_fields());
    let mut facts = ChunkFacts::new();
    let request = Request::new(Columns::Indices(&[1]), 1..2).with_location();
    metadata.extract(&request, &mut facts).unwrap();
    assert_eq!((facts.row_group_count(), facts.column_count()), (1, 1));
    assert_eq!(facts.location(slot(0, 0)).unwrap().start(), 254);
    assert!(facts.location(slot(0, 1)).is_none());

    // Columns come back in the order the request lists them, repeats included.
    let repeated = Request::new(Columns::Indices(&[1, 0, 1]), 0..2).with_location();
    metadata.extract(&repeated, &mut facts).unwrap();
    assert_eq!((facts.row_group_count(), facts.column_count()), (2, 3));
    let starts: Vec<_> = (0..2)
        .flat_map(|row_group| (0..3).map(move |column| slot(row_group, column)))
        .map(|slot| facts.location(slot).unwrap().start())
        .collect();
    assert_eq!(starts, [104, 4, 104, 254, 154, 254]);

    // A request for no row groups, or for no facts, extracts nothing and succeeds.
    metadata
        .extract(
            &Request::new(Columns::All, 2..2).with_location(),
            &mut facts,
        )
        .unwrap();
    assert_eq!(facts.row_group_count(), 0);
    metadata
        .extract(&Request::new(Columns::All, 0..2), &mut facts)
        .unwrap();
    assert!(facts.location(slot(0, 0)).is_none());
}

#[test]
fn a_request_outside_the_file_is_an_error() {
    let (metadata, _) = opened(simple_fields());
    let mut facts = ChunkFacts::new();
    assert_eq!(
        metadata.extract(
            &Request::new(Columns::All, 1..3).with_location(),
            &mut facts
        ),
        Err(ExtractError::RowGroups {
            start: 1,
            end: 3,
            count: 2
        })
    );
    #[expect(
        clippy::reversed_empty_ranges,
        reason = "the reversed range is the case"
    )]
    let backwards = Request::new(Columns::All, 2..1).with_location();
    assert!(matches!(
        metadata.extract(&backwards, &mut facts),
        Err(ExtractError::RowGroups { .. })
    ));
    assert_eq!(
        metadata.extract(
            &Request::new(Columns::Indices(&[0, 2]), 0..1).with_location(),
            &mut facts
        ),
        Err(ExtractError::Column {
            column: 2,
            count: 2
        })
    );
}

#[test]
fn a_malformed_chunk_fails_only_the_requests_that_cover_it() {
    // The second column's chunk in the second row group has a negative value count.
    let bad = with_field(column_metadata(254, 60), 5, Value::I64(-1));
    let row_groups = structs(vec![
        row_group(
            vec![
                chunk(column_metadata(4, 100)),
                chunk(column_metadata(104, 50)),
            ],
            10,
        ),
        row_group(vec![chunk(column_metadata(154, 100)), chunk(bad)], 7),
    ]);
    // Opening does not decode column chunks, so the file opens.
    let (metadata, _) = opened(with_field(simple_fields(), 4, row_groups));
    let mut facts = ChunkFacts::new();
    metadata
        .extract(
            &Request::new(Columns::Indices(&[0]), 0..2).with_location(),
            &mut facts,
        )
        .unwrap();
    metadata
        .extract(
            &Request::new(Columns::All, 0..1).with_location(),
            &mut facts,
        )
        .unwrap();
    assert_eq!(facts.location(slot(0, 1)).unwrap().start(), 104);

    let error = metadata
        .extract(&everything(&metadata), &mut facts)
        .unwrap_err();
    assert!(matches!(
        error,
        ExtractError::ColumnChunk {
            row_group: 1,
            column: 1,
            source: FooterError::InvalidValue {
                structure: Structure::ColumnMetaData,
                field: 5,
                ..
            }
        }
    ));
    // A failed extraction leaves nothing to read, old or new.
    assert_eq!((facts.row_group_count(), facts.column_count()), (0, 0));
    assert!(facts.location(slot(0, 0)).is_none());
}

#[test]
fn only_the_requested_facts_are_decoded() {
    // Statistics with a negative null count, in a chunk whose location is fine.
    let mut fields = column_metadata(4, 100);
    fields.push((12, Value::Struct(vec![(3, Value::I64(-1))])));
    let metadata = one_chunk(fields);
    let mut facts = ChunkFacts::new();
    metadata
        .extract(
            &Request::new(Columns::All, 0..1).with_location(),
            &mut facts,
        )
        .unwrap();
    assert_eq!(facts.location(slot(0, 0)).unwrap().start(), 4);
    let statistics = Request::new(Columns::All, 0..1).with_statistics();
    assert!(matches!(
        chunk_error(&metadata, &statistics),
        FooterError::InvalidValue {
            structure: Structure::Statistics,
            field: 3,
            ..
        }
    ));
}

#[test]
fn each_required_location_field_is_required() {
    for field in [2, 4, 5, 6, 7, 9] {
        let metadata = one_chunk(without_field(column_metadata(4, 100), i16::from(field)));
        assert_eq!(
            chunk_error(&metadata, &everything(&metadata)),
            FooterError::MissingField {
                structure: Structure::ColumnMetaData,
                field
            }
        );
        // Statistics alone do not need it.
        let statistics = Request::new(Columns::All, 0..1).with_statistics();
        metadata
            .extract(&statistics, &mut ChunkFacts::new())
            .unwrap();
    }
    let no_metadata = one_chunk_struct(Value::Struct(vec![(2, Value::I64(0))]));
    assert_eq!(
        chunk_error(&no_metadata, &everything(&no_metadata)),
        FooterError::MissingField {
            structure: Structure::ColumnChunk,
            field: 3
        }
    );
}

#[test]
fn invalid_chunk_values_are_rejected() {
    let invalid = |metadata: Vec<(i16, Value)>, field: u8| {
        let metadata = one_chunk(metadata);
        assert!(matches!(
            chunk_error(&metadata, &everything(&metadata)),
            FooterError::InvalidValue { structure: Structure::ColumnMetaData, field: found, .. }
                if found == field
        ));
    };
    // A codec and an encoding the format does not have.
    invalid(with_field(column_metadata(4, 1), 4, Value::I32(8)), 4);
    let encodings = |codes: Vec<i32>| {
        Value::List(
            ElementType::I32,
            codes.into_iter().map(Value::I32).collect(),
        )
    };
    for bad in [vec![1], vec![11], vec![-1], vec![0, 64]] {
        invalid(with_field(column_metadata(4, 1), 2, encodings(bad)), 2);
    }
    let strings = Value::List(ElementType::Binary, vec![text("PLAIN")]);
    invalid(with_field(column_metadata(4, 1), 2, strings), 2);
    // A chunk that starts or ends outside the file.
    let beyond = i64::try_from(FILE_SIZE).unwrap();
    invalid(column_metadata(beyond + 1, 1), 9);
    invalid(column_metadata(beyond - 10, 11), 7);
    invalid(column_metadata(4, i64::MAX), 7);
}

#[test]
fn a_repeated_or_mistyped_chunk_field_is_handled_as_in_the_footer() {
    let mut repeated = column_metadata(4, 100);
    repeated.push((11, Value::I64(2)));
    repeated.push((5, Value::I64(1)));
    let metadata = one_chunk(repeated);
    // The repeat comes after every field a location needs, so a location request stops
    // before it; asking for statistics as well reads to the end of the structure, since
    // the chunk has none, and finds it.
    let location = Request::new(Columns::All, 0..1).with_location();
    metadata.extract(&location, &mut ChunkFacts::new()).unwrap();
    assert!(matches!(
        chunk_error(&metadata, &everything(&metadata)),
        FooterError::DuplicateField {
            structure: Structure::ColumnMetaData,
            field: 5,
            ..
        }
    ));
    // A value count that is an `i32` is skipped, so the field is missing.
    let mistyped = one_chunk(with_field(column_metadata(4, 100), 5, Value::I32(100)));
    assert_eq!(
        chunk_error(&mistyped, &everything(&mistyped)),
        FooterError::MissingField {
            structure: Structure::ColumnMetaData,
            field: 5
        }
    );
}

#[test]
fn a_chunk_in_another_file_or_an_encrypted_chunk_is_unsupported() {
    let external = Value::Struct(vec![
        (1, text("other.parquet")),
        (2, Value::I64(0)),
        (3, Value::Struct(column_metadata(4, 100))),
    ]);
    let metadata = one_chunk_struct(external);
    assert!(matches!(
        chunk_error(&metadata, &everything(&metadata)),
        FooterError::Unsupported {
            feature: Unsupported::ExternalFile,
            ..
        }
    ));
    let encrypted = Value::Struct(vec![(2, Value::I64(0)), (9, text("ciphertext"))]);
    let encrypted = one_chunk_struct(encrypted);
    assert!(matches!(
        chunk_error(&encrypted, &everything(&encrypted)),
        FooterError::Unsupported {
            feature: Unsupported::Encryption,
            ..
        }
    ));
}

#[test]
fn storage_is_reused_between_files() {
    let (wide, _) = opened(simple_fields());
    let narrow = one_chunk(column_metadata(4, 100));
    let mut facts = ChunkFacts::new();
    wide.extract(&everything(&wide), &mut facts).unwrap();
    assert_eq!((facts.row_group_count(), facts.column_count()), (2, 2));
    narrow.extract(&everything(&narrow), &mut facts).unwrap();
    assert_eq!((facts.row_group_count(), facts.column_count()), (1, 1));
    assert_eq!(facts.location(slot(0, 0)).unwrap().start(), 4);
    assert!(
        facts.location(slot(1, 0)).is_none(),
        "nothing of the first file is left"
    );
    // Statistics positions belong to the file they came from.
    let statistics = *facts.statistics(slot(0, 0)).unwrap();
    assert_eq!(statistics.min(&wide), None);
}

// ---------------------------------------------------------------------------
// Real files, against arrow-rs
// ---------------------------------------------------------------------------

fn arrow_physical_type(physical_type: PhysicalType) -> ArrowType {
    match physical_type {
        PhysicalType::Boolean => ArrowType::BOOLEAN,
        PhysicalType::Int32 => ArrowType::INT32,
        PhysicalType::Int64 => ArrowType::INT64,
        PhysicalType::Int96 => ArrowType::INT96,
        PhysicalType::Float => ArrowType::FLOAT,
        PhysicalType::Double => ArrowType::DOUBLE,
        PhysicalType::ByteArray => ArrowType::BYTE_ARRAY,
        PhysicalType::FixedLenByteArray => ArrowType::FIXED_LEN_BYTE_ARRAY,
    }
}

/// Whether a bound stored in the footer is the one arrow-rs reports. arrow-rs parses a
/// fixed-width bound into a value and reports that value's bytes, so a stored bound with
/// extra bytes, or a boolean stored as something other than 0 or 1, differs in its bytes
/// but not in its value.
fn same_bound(physical_type: PhysicalType, ours: Option<&[u8]>, theirs: Option<&[u8]>) -> bool {
    match (ours, theirs) {
        (None, None) => true,
        (Some(ours), Some(theirs)) => match physical_type {
            PhysicalType::Boolean => (ours[0] != 0) == (theirs[0] != 0),
            _ => ours.starts_with(theirs),
        },
        _ => false,
    }
}

fn assert_schema_matches(name: &str, schema: &Schema, arrow: &ParquetMetaData) {
    let arrow_schema = arrow.file_metadata().schema_descr();
    assert_eq!(
        schema.column_count() as usize,
        arrow_schema.num_columns(),
        "{name}: columns"
    );
    for (index, theirs) in (0..).zip(arrow_schema.columns()) {
        let ours = schema.column(index).unwrap();
        let context = format!("{name}: column {index}");
        assert_eq!(ours.path(), theirs.path().parts(), "{context}: path");
        assert_eq!(
            arrow_physical_type(ours.physical_type().unwrap()),
            theirs.physical_type(),
            "{context}: physical type"
        );
        assert_eq!(
            i16::from(ours.definition_level_max()),
            theirs.max_def_level(),
            "{context}: definition level"
        );
        assert_eq!(
            i16::from(ours.repetition_level_max()),
            theirs.max_rep_level(),
            "{context}: repetition level"
        );
        if ours.physical_type() == Some(PhysicalType::FixedLenByteArray) {
            let length = i32::try_from(ours.type_length().unwrap()).unwrap();
            assert_eq!(length, theirs.type_length(), "{context}: type length");
        }
        assert_eq!(
            ours.logical_type().is_some(),
            theirs.logical_type_ref().is_some()
                || theirs.converted_type() != parquet::basic::ConvertedType::NONE,
            "{context}: has a logical type"
        );
    }
}

fn assert_chunks_match(name: &str, metadata: &Metadata, schema: &Schema, arrow: &ParquetMetaData) {
    let mut facts = ChunkFacts::new();
    metadata
        .extract(&everything(metadata), &mut facts)
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    for (row_group, arrow_row_group) in (0..).zip(arrow.row_groups()) {
        assert_eq!(
            metadata.row_group_row_count(row_group),
            u64::try_from(arrow_row_group.num_rows()).ok(),
            "{name}: row group {row_group} rows"
        );
        assert_eq!(
            metadata.row_group_byte_size(row_group),
            u64::try_from(arrow_row_group.total_byte_size()).ok(),
            "{name}: row group {row_group} bytes"
        );
        for (column, theirs) in (0..).zip(arrow_row_group.columns()) {
            let context = format!("{name}: row group {row_group}, column {column}");
            let location = facts.location(slot(row_group, column)).unwrap();
            assert_location_matches(&context, location, theirs);
            let physical_type = schema.column(column).unwrap().physical_type().unwrap();
            let statistics = facts.statistics(slot(row_group, column)).unwrap();
            if let Some(theirs) = theirs.statistics() {
                assert_statistics_match(&context, metadata, physical_type, statistics, theirs);
            }
        }
    }
}

fn assert_location_matches(context: &str, ours: &Location, theirs: &ColumnChunkMetaData) {
    let (start, size) = theirs.byte_range();
    let sizes = (
        ours.compressed_size(),
        ours.uncompressed_size(),
        ours.value_count(),
        ours.data_page_offset(),
    );
    let expected = (
        size,
        u64::try_from(theirs.uncompressed_size()).unwrap(),
        u64::try_from(theirs.num_values()).unwrap(),
        u64::try_from(theirs.data_page_offset()).unwrap(),
    );
    assert_eq!(sizes, expected, "{context}: sizes, values, data page");
    // arrow-rs starts a chunk at its dictionary page offset whatever it is; the two
    // agree wherever that offset is before the data.
    if theirs
        .dictionary_page_offset()
        .is_none_or(|dictionary| dictionary > 0 && dictionary < theirs.data_page_offset())
    {
        assert_eq!(ours.start(), start, "{context}: start");
    }
    assert_eq!(
        format!("{:?}", ours.codec()).to_uppercase(),
        format!("{:?}", theirs.compression_codec()).replace('_', ""),
        "{context}: codec"
    );
}

fn assert_statistics_match(
    context: &str,
    metadata: &Metadata,
    physical_type: PhysicalType,
    ours: &Statistics,
    theirs: &ArrowStatistics,
) {
    assert_eq!(
        (ours.null_count(), ours.distinct_count()),
        (theirs.null_count_opt(), theirs.distinct_count_opt()),
        "{context}: null and distinct counts"
    );
    assert!(
        same_bound(physical_type, ours.min(metadata), theirs.min_bytes_opt()),
        "{context}: minimum"
    );
    assert!(
        same_bound(physical_type, ours.max(metadata), theirs.max_bytes_opt()),
        "{context}: maximum"
    );
    let has_bound = theirs.min_bytes_opt().is_some() || theirs.max_bytes_opt().is_some();
    assert_eq!(
        ours.bounds_are_deprecated(),
        theirs.is_min_max_deprecated() && has_bound,
        "{context}: deprecated bounds"
    );
}

/// The files of parquet-testing, by name, with their bytes.
fn parquet_testing_files() -> Vec<(String, Vec<u8>)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/parquet-testing/data");
    let mut paths: Vec<_> = fs::read_dir(&directory)
        .unwrap_or_else(|error| {
            panic!(
                "{}: {error} (run `git submodule update --init`)",
                directory.display()
            )
        })
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "parquet")
        })
        .collect();
    paths.sort();
    assert!(paths.len() > 50, "found {} files", paths.len());
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read(&path).unwrap())
        })
        .collect()
}

/// The footer of a file, by its trailer.
fn footer_of(file: &[u8]) -> Option<&[u8]> {
    let (rest, trailer) = file.split_last_chunk::<8>()?;
    let size = u32::from_le_bytes(trailer[..4].try_into().unwrap());
    rest.get(rest.len().checked_sub(size as usize)?..)
}

#[test]
fn every_parquet_testing_file_matches_arrow_rs() {
    let mut compared = 0;
    let mut rejected_by_us = Vec::new();
    for (name, file) in parquet_testing_files() {
        let file_size = file.len() as u64;
        let ours = open(&file, file_size, &Limits::new(), None);
        let theirs = footer_of(&file).map(ParquetMetaDataReader::decode_metadata);
        let Some(Ok(arrow)) = theirs else {
            // A file arrow-rs cannot read: we must only not panic, here or in extraction.
            if let Ok(Opened::File { metadata, .. }) = ours {
                let _ = metadata.extract(&everything(&metadata), &mut ChunkFacts::new());
            }
            continue;
        };
        let (metadata, schema) = match ours {
            Ok(Opened::File { metadata, schema }) => (metadata, schema.unwrap()),
            Ok(Opened::MissingBytes { .. }) => panic!("{name}: the whole file was supplied"),
            Err(error) => {
                rejected_by_us.push(format!("{name}: {error}"));
                continue;
            }
        };
        let file_metadata = arrow.file_metadata();
        assert_eq!(
            metadata.version(),
            file_metadata.version(),
            "{name}: version"
        );
        assert_eq!(
            metadata.row_count(),
            u64::try_from(file_metadata.num_rows()).unwrap(),
            "{name}: rows"
        );
        assert_eq!(
            metadata.created_by(),
            Ok(file_metadata.created_by()),
            "{name}"
        );
        assert_eq!(
            metadata.row_group_count() as usize,
            arrow.num_row_groups(),
            "{name}: row groups"
        );
        assert_eq!(
            metadata.key_value_metadata().count(),
            file_metadata.key_value_metadata().map_or(0, Vec::len),
            "{name}: key-value pairs"
        );
        assert_schema_matches(&name, &schema, &arrow);
        assert_chunks_match(&name, &metadata, &schema, &arrow);
        compared += 1;
    }
    // Files arrow-rs reads and we do not: only the ones with encrypted columns, which
    // arrow-rs reads the plaintext footer of when built without encryption.
    for rejection in &rejected_by_us {
        assert!(
            rejection.contains("encryption is not supported"),
            "unexpected rejection: {rejection}"
        );
    }
    assert!(compared > 50, "compared {compared} files");
}

#[test]
fn written_files_of_many_row_groups_match_arrow_rs() {
    // The files of parquet-testing have few row groups. These have enough for column
    // chunks to be passed over by comparing them with earlier ones: chunks that differ
    // only in their offsets, chunks of random data, and chunks that change shape in
    // every row group.
    let files = [
        ("uniform", corpus::uniform_file(12, 9)),
        ("varied", corpus::varied_file(18, 14)),
        ("alternating", corpus::alternating_file(6, 9)),
    ];
    for (name, file) in files {
        let arrow = ParquetMetaDataReader::decode_metadata(footer_of(&file).unwrap()).unwrap();
        let file_size = file.len() as u64;
        let Opened::File { metadata, schema } =
            open(&file, file_size, &Limits::new(), None).unwrap()
        else {
            panic!("{name}: the whole file was supplied");
        };
        let schema = schema.unwrap();
        assert_eq!(
            metadata.row_group_count() as usize,
            arrow.num_row_groups(),
            "{name}: row groups"
        );
        assert_schema_matches(name, &schema, &arrow);
        assert_chunks_match(name, &metadata, &schema, &arrow);
    }
}

#[test]
fn every_parquet_testing_file_opens_the_same_with_its_own_schema_supplied() {
    for (name, file) in parquet_testing_files() {
        let file_size = file.len() as u64;
        let Ok(Opened::File { metadata, schema }) = open(&file, file_size, &Limits::new(), None)
        else {
            continue;
        };
        let schema = schema.unwrap();
        let Opened::File {
            metadata: again,
            schema: decoded,
        } = open(&file, file_size, &Limits::new(), Some(&schema)).unwrap()
        else {
            panic!("{name}: the whole file was supplied");
        };
        // Every real writer puts the version and then the schema first, so the schema is
        // always recognized.
        assert!(decoded.is_none(), "{name}: schema decoded again");
        assert_eq!(again.version(), metadata.version(), "{name}");
        assert_eq!(again.row_count(), metadata.row_count(), "{name}");
        assert_eq!(again.column_count(), metadata.column_count(), "{name}");
        assert_eq!(
            again.row_group_count(),
            metadata.row_group_count(),
            "{name}"
        );
        assert_eq!(again.created_by(), metadata.created_by(), "{name}");
        let (mut first, mut second) = (ChunkFacts::new(), ChunkFacts::new());
        let ours = metadata.extract(&everything(&metadata), &mut first);
        assert_eq!(
            again.extract(&everything(&again), &mut second),
            ours,
            "{name}"
        );
        for row_group in 0..first.row_group_count() {
            for column in 0..first.column_count() {
                let slot = slot(row_group, column);
                assert_eq!(first.location(slot), second.location(slot), "{name}");
                assert_eq!(first.statistics(slot), second.statistics(slot), "{name}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Passing over column chunks by comparing them
// ---------------------------------------------------------------------------

/// A column chunk at `offset` whose statistics have `bound` as both bounds.
fn chunk_bounded(offset: i64, bound: &[u8]) -> Value {
    chunk_sized(offset, 50, bound)
}

/// [`chunk_bounded`] for a chunk of `size` bytes.
fn chunk_sized(offset: i64, size: i64, bound: &[u8]) -> Value {
    let mut metadata = column_metadata(offset, size);
    metadata.push((
        12,
        Value::Struct(vec![
            (3, Value::I64(0)),
            (5, Value::Binary(bound.to_vec())),
            (6, Value::Binary(bound.to_vec())),
        ]),
    ));
    chunk(metadata)
}

/// A column chunk at `offset` that has a dictionary page, and so a field that
/// [`chunk_bounded`] has not, and no statistics.
fn chunk_with_dictionary(offset: i64) -> Value {
    let mut metadata = column_metadata(offset, 50);
    metadata.push((11, Value::I64(offset)));
    chunk(metadata)
}

/// Chunks laid end to end, as a row group's list holds them, and where each starts,
/// with the end of the last.
fn laid_out(chunks: &[Value]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut bounds = vec![0];
    for chunk in chunks {
        bytes.extend(encoded(chunk));
        bounds.push(bytes.len());
    }
    (bytes, bounds)
}

/// Where the struct at `start` ends, by parsing it.
fn parsed_end(bytes: &[u8], start: usize) -> Result<usize, FooterError> {
    let mut reader = CompactReader::new(bytes, start);
    reader.skip(ElementType::Struct)?;
    Ok(reader.position())
}

/// Passes over the chunks of one column, one from each of as many row groups, with
/// credit to spare, and checks that each is found to end where it does.
fn passed_over(chunks: &[Value]) -> Templates {
    let (bytes, bounds) = laid_out(chunks);
    let mut templates = Templates::new(1 << 20, u32::try_from(chunks.len()).unwrap());
    for bound in bounds.windows(2) {
        templates.row_group_begin();
        assert_eq!(templates.chunk_end(0, &bytes, bound[0]), Ok(bound[1]));
    }
    templates
}

#[test]
fn chunks_that_differ_only_in_values_are_passed_over_by_comparing() {
    let chunks = [
        chunk_bounded(100, b"aa"),
        chunk_bounded(150, b"zz"),
        chunk_bounded(200, b"mm"),
        chunk_bounded(250, b"qq"),
    ];
    let templates = passed_over(&chunks);
    assert_eq!(templates.hit_count(0), 3);
    assert_eq!(templates.miss_count(0), 0);
    assert_eq!(templates.variable_count(0), 0);
}

#[test]
fn integers_and_binaries_of_other_lengths_still_match() {
    let chunks = [
        chunk_bounded(100, b"aa"),
        // A longer offset and longer bounds.
        chunk_bounded(1_000_000, b"a bound much longer than the first"),
        // A shorter offset and empty bounds.
        chunk_bounded(3, b""),
        // The longest offset there is, and bounds whose lengths take two bytes.
        chunk_bounded(i64::MAX, &[0xFF; 300]),
    ];
    let templates = passed_over(&chunks);
    assert_eq!(templates.hit_count(0), 3);
    assert_eq!(templates.miss_count(0), 0);
    // The template has learned that the offset and the two bounds vary in length.
    assert_eq!(templates.variable_count(0), 3);
}

#[test]
fn an_integer_that_has_grown_replaces_the_template_when_many_row_groups_follow() {
    // An offset one byte longer than the template's, as a file's offsets get.
    let chunks = [chunk_bounded(100, b"aa"), chunk_bounded(1_000_000, b"bb")];
    let (bytes, bounds) = laid_out(&chunks);
    for (row_groups, replaced) in [(100, true), (10, false)] {
        let mut templates = Templates::new(1 << 20, row_groups);
        for bound in bounds.windows(2) {
            templates.row_group_begin();
            assert_eq!(templates.chunk_end(0, &bytes, bound[0]), Ok(bound[1]));
        }
        assert_eq!(templates.hit_count(0), 1);
        if replaced {
            // With many row groups to come, the second chunk is the template now.
            assert_eq!(templates.template_start(0), Some(bounds[1]));
            assert_eq!(templates.variable_count(0), 0);
        } else {
            // With few, the first stays, and remembers that the offset varies.
            assert_eq!(templates.template_start(0), Some(bounds[0]));
            assert_eq!(templates.variable_count(0), 1);
        }
    }
}

#[test]
fn a_template_with_no_room_for_more_varying_lengths_is_replaced() {
    // The second chunk differs from the first in the lengths of its two sizes, its
    // offset, and its two bounds: one more than a template remembers.
    let chunks = [
        chunk_sized(100, 50, b"aa"),
        chunk_sized(1_100_000, 1_100_000, b"a bound much longer than the first"),
        chunk_sized(1_200_000, 1_200_000, b"a bound just as long as the second"),
        chunk_sized(1_300_000, 1_300_000, b"a bound just as long as the others"),
    ];
    let (bytes, bounds) = laid_out(&chunks);
    let mut templates = Templates::new(1 << 20, 4);
    for (index, bound) in bounds.windows(2).enumerate() {
        templates.row_group_begin();
        assert_eq!(templates.chunk_end(0, &bytes, bound[0]), Ok(bound[1]));
        // From the second chunk on, the second chunk is the template, and the chunks
        // after it are like it.
        let template = bounds[index.min(1)];
        assert_eq!(templates.template_start(0), Some(template));
        assert_eq!(templates.variable_count(0), 0);
    }
    assert_eq!(templates.hit_count(0), 3);
    assert_eq!(templates.miss_count(0), 0);
}

#[test]
fn a_chunk_with_other_fields_is_parsed_and_becomes_the_template() {
    let chunks = [
        chunk_bounded(100, b"aa"),
        chunk_bounded(200, b"bb"),
        chunk_with_dictionary(100),
        chunk_with_dictionary(200),
        chunk_with_dictionary(300),
        chunk_with_dictionary(400),
    ];
    let templates = passed_over(&chunks);
    // The third differs from the chunk before, and the last three are like it.
    assert_eq!(templates.miss_count(0), 1);
    assert_eq!(templates.hit_count(0), 4);
    assert!(templates.has_template(0));
}

/// Passes over two columns' chunks, row group by row group: `chunks` holds the first
/// column's chunk and then the second's for each row group.
fn passed_over_in_pairs(chunks: &[Value]) -> Templates {
    let (bytes, bounds) = laid_out(chunks);
    let row_groups = u32::try_from(chunks.len() / 2).unwrap();
    let mut templates = Templates::new(bytes.len(), row_groups);
    for (index, bound) in bounds.windows(2).enumerate() {
        let column = u32::try_from(index % 2).unwrap();
        if column == 0 {
            templates.row_group_begin();
        }
        assert_eq!(templates.chunk_end(column, &bytes, bound[0]), Ok(bound[1]));
    }
    templates
}

#[test]
fn a_column_that_never_matches_gets_no_second_template_unless_matches_pay_for_it() {
    // One column alone, with other fields in every row group. Its first template is
    // paid for by the grant; nothing ever matches, so nothing pays for another.
    let chunks: Vec<Value> = (0..8)
        .map(|index| {
            if index % 2 == 0 {
                chunk_bounded(100, b"aa")
            } else {
                chunk_with_dictionary(100)
            }
        })
        .collect();
    let templates = passed_over(&chunks);
    assert_eq!(templates.hit_count(0), 0);
    assert_eq!(templates.miss_count(0), 1);
    assert!(!templates.has_template(0));
}

#[test]
fn a_column_that_keeps_changing_stops_being_compared() {
    // The first column's chunks are alike, and its matches pay for templates. The
    // second column's chunks change in every row group.
    let chunks: Vec<Value> = (0..10)
        .flat_map(|row_group| {
            let changing = if row_group % 2 == 0 {
                chunk_bounded(100, b"aa")
            } else {
                chunk_with_dictionary(100)
            };
            [chunk_bounded(100 + row_group, b"aa"), changing]
        })
        .collect();
    let templates = passed_over_in_pairs(&chunks);
    assert_eq!(templates.hit_count(0), 9);
    // Three chunks were compared and differed. After that none was compared.
    assert_eq!(templates.hit_count(1), 0);
    assert_eq!(templates.miss_count(1), 3);
    assert!(!templates.has_template(1));
}

#[test]
fn templates_cost_no_more_than_their_credit() {
    // Two columns in four row groups. The grant is an eighth of these bytes, which
    // one template uses up.
    let chunks: Vec<Value> = (0..8)
        .map(|index| chunk_bounded(100 + index, b"aa"))
        .collect();
    let (bytes, bounds) = laid_out(&chunks);
    let mut templates = Templates::new(bytes.len(), 4);
    let row_group = |passing: &mut Templates, first: usize| {
        passing.row_group_begin();
        for column in 0..2 {
            let start = bounds[first + column];
            let end = bounds[first + column + 1];
            let column = u32::try_from(column).unwrap();
            assert_eq!(passing.chunk_end(column, &bytes, start), Ok(end));
        }
    };
    row_group(&mut templates, 0);
    assert!(templates.has_template(0));
    assert!(!templates.has_template(1));
    // The first column's chunk matches, and what that saves pays for the second
    // column's template.
    row_group(&mut templates, 2);
    assert_eq!(templates.hit_count(0), 1);
    assert!(templates.has_template(1));
    row_group(&mut templates, 4);
    row_group(&mut templates, 6);
    assert_eq!(templates.hit_count(0), 3);
    assert_eq!(templates.hit_count(1), 2);
}

#[test]
fn the_last_row_group_gets_no_template() {
    let chunks = [
        chunk_bounded(100, b"aa"),
        chunk_bounded(200, b"bb"),
        chunk_bounded(300, b"cc"),
        chunk_with_dictionary(100),
    ];
    let templates = passed_over(&chunks);
    assert_eq!(templates.hit_count(0), 2);
    assert_eq!(templates.miss_count(0), 1);
    // Nothing comes after the last chunk to be compared with it.
    assert!(!templates.has_template(0));
}

#[test]
fn a_footer_in_which_nothing_matches_is_parsed_from_then_on() {
    // One column whose chunk has other fields in every row group.
    let chunks: Vec<Value> = (0..6)
        .map(|index| {
            if index % 2 == 0 {
                chunk_bounded(100, b"aa")
            } else {
                chunk_with_dictionary(100)
            }
        })
        .collect();
    let (bytes, bounds) = laid_out(&chunks);
    let mut templates = Templates::new(1 << 20, 6);
    for (row_group, bound) in bounds.windows(2).enumerate() {
        let mut chunk_positions = [u32::MAX];
        let end = templates.chunks_locate(&bytes, bound[0], &mut chunk_positions);
        assert_eq!(end, Ok(bound[1]));
        assert_eq!(chunk_positions, [u32::try_from(bound[0]).unwrap()]);
        // The second chunk did not match the first, and no match has paid for another
        // template: there is nothing left to compare with.
        assert_eq!(templates.is_idle(), row_group >= 1);
    }
    assert_eq!(templates.miss_count(0), 1);
}

#[test]
fn a_chunk_that_parsing_rejects_is_rejected_the_same() {
    let template = encoded(&chunk_bounded(100, b"aaaa"));
    let size = template.len();
    let bound_length = template
        .windows(5)
        .position(|window| window == [4, b'a', b'a', b'a', b'a'])
        .unwrap();
    // The footer ends inside the chunk.
    let cut = template[..size - 3].to_vec();
    // A bound claims more bytes than the footer has left.
    let mut overlong = template.clone();
    overlong[bound_length] = 0x7F;
    // The first field has a type that does not exist.
    let mut mistyped = template.clone();
    mistyped[0] |= 0x0F;
    // An integer never ends.
    let mut unending = template.clone();
    let last = unending.len() - 1;
    unending[1..=last].fill(0x80);
    for damaged in [cut, overlong, mistyped, unending] {
        let mut bytes = template.clone();
        bytes.extend(damaged);
        let mut templates = Templates::new(1 << 20, 4);
        templates.row_group_begin();
        assert_eq!(templates.chunk_end(0, &bytes, 0), Ok(size));
        templates.row_group_begin();
        let error = parsed_end(&bytes, size).unwrap_err();
        assert_eq!(templates.chunk_end(0, &bytes, size), Err(error));
    }
}

/// How the column chunks of a written file fare when they are passed over as the pass
/// that opens a file passes over them.
#[derive(Debug, PartialEq, Eq)]
struct Passed {
    /// Chunks after the first row group, which are the ones that can match.
    later_chunks: u32,
    /// Chunks that matched a template.
    hits: u32,
    /// Chunks that were compared with a template and did not match.
    misses: u32,
    /// Chunks that matched only by being compared a second time.
    stepped: u32,
}

/// Passes over every column chunk of a file's footer, row group by row group.
fn passed(file: &[u8]) -> Passed {
    let footer = footer_of(file).unwrap();
    let mut reader = CompactReader::new(footer, 0);
    let mut fields = FieldCursor::default();
    while let Some(field) = fields.next(&mut reader).unwrap() {
        if field.id() != 4 {
            reader.skip_field(field.field_type()).unwrap();
            continue;
        }
        let row_groups = reader.read_list_header().unwrap();
        let footer_size_left = footer.len() - reader.position();
        let mut templates = Templates::new(footer_size_left, row_groups.size());
        let mut column_count = 0;
        for _ in 0..row_groups.size() {
            let mut row_group_fields = FieldCursor::default();
            while let Some(row_group_field) = row_group_fields.next(&mut reader).unwrap() {
                if row_group_field.id() != 1 {
                    reader.skip_field(row_group_field.field_type()).unwrap();
                    continue;
                }
                let chunks = reader.read_list_header().unwrap();
                column_count = chunks.size();
                templates.row_group_begin();
                let mut position = reader.position();
                for column in 0..column_count {
                    position = templates.chunk_end(column, footer, position).unwrap();
                }
                reader = CompactReader::new(footer, position);
            }
        }
        return Passed {
            later_chunks: column_count * (row_groups.size() - 1),
            hits: (0..column_count)
                .map(|column| templates.hit_count(column))
                .sum(),
            misses: (0..column_count)
                .map(|column| templates.miss_count(column))
                .sum(),
            stepped: templates.stepped_count(),
        };
    }
    panic!("the footer has no row groups");
}

#[test]
fn written_row_groups_are_passed_over_by_comparing() {
    // Row groups that differ only in their file offsets, as arrow-rs writes them. An
    // offset grows by a byte now and then, which the templates learn to step over.
    let alike = passed(&corpus::uniform_file(20, 40));
    assert_eq!(alike.hits, alike.later_chunks);
    assert_eq!(alike.misses, 0);
    assert!(alike.stepped * 10 <= alike.later_chunks);
    // Random values, strings of random lengths, and random nulls change the lengths of
    // integers and of bounds from one row group to the next, and nothing else. The
    // templates learn which, so few chunks need comparing twice.
    let varied = passed(&corpus::varied_file(30, 40));
    assert_eq!(varied.hits, varied.later_chunks);
    assert_eq!(varied.misses, 0);
    assert!(varied.stepped * 10 <= varied.later_chunks);
    // Chunks with other fields in every row group never match. Each column gets one
    // template, is compared with it once, and is then left alone, however many row
    // groups follow.
    let alternating = passed(&corpus::alternating_file(10, 100));
    assert_eq!(alternating.hits, 0);
    assert_eq!(alternating.misses, 10);
    assert_eq!(alternating.stepped, 0);
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// What a generated footer says about one column chunk.
#[derive(Debug, Clone)]
struct ChunkModel {
    data_page_offset: u32,
    dictionary_gap: Option<u8>,
    compressed_size: u32,
    value_count: u32,
    codec: u8,
    null_count: Option<u32>,
    min: Option<Vec<u8>>,
    max: Option<Vec<u8>>,
}

impl ChunkModel {
    fn start(&self) -> u64 {
        let gap = self
            .dictionary_gap
            .filter(|gap| *gap > 0)
            .map_or(0, u64::from);
        u64::from(self.data_page_offset) - gap
    }

    /// This chunk's values in the fields `shape` has: every optional field is present
    /// exactly when it is in `shape`.
    fn shaped_like(mut self, shape: &Self) -> Self {
        self.dictionary_gap = shape
            .dictionary_gap
            .map(|_| self.dictionary_gap.unwrap_or(1));
        self.null_count = shape.null_count.map(|_| self.null_count.unwrap_or(0));
        self.min = shape
            .min
            .as_ref()
            .map(|_| self.min.clone().unwrap_or_default());
        self.max = shape
            .max
            .as_ref()
            .map(|_| self.max.clone().unwrap_or_default());
        self
    }

    fn to_value(&self, unknown_field: bool) -> Value {
        let mut statistics = Vec::new();
        if let Some(null_count) = self.null_count {
            statistics.push((3, Value::I64(i64::from(null_count))));
        }
        if let Some(max) = &self.max {
            statistics.push((5, Value::Binary(max.clone())));
        }
        if let Some(min) = &self.min {
            statistics.push((6, Value::Binary(min.clone())));
        }
        let mut metadata = vec![
            (1, Value::I32(1)),
            (2, Value::List(ElementType::I32, vec![Value::I32(8)])),
            (4, Value::I32(i32::from(self.codec))),
            (5, Value::I64(i64::from(self.value_count))),
            (6, Value::I64(i64::from(self.compressed_size) * 3)),
            (7, Value::I64(i64::from(self.compressed_size))),
            (9, Value::I64(i64::from(self.data_page_offset))),
        ];
        if let Some(gap) = self.dictionary_gap {
            let offset = i64::from(self.data_page_offset) - i64::from(gap);
            metadata.push((11, Value::I64(offset)));
        }
        metadata.push((12, Value::Struct(statistics)));
        if unknown_field {
            metadata.push((60, Value::List(ElementType::I64, vec![Value::I64(-7)])));
        }
        chunk(metadata)
    }
}

fn chunk_model() -> impl Strategy<Value = ChunkModel> {
    let bound = || prop::option::of(prop::collection::vec(any::<u8>(), 0..20));
    (
        (1000_u32..1_000_000, prop::option::of(0_u8..200)),
        (0_u32..1_000_000, any::<u32>(), 0_u8..8),
        (prop::option::of(any::<u32>()), bound(), bound()),
    )
        .prop_map(
            |((data_page_offset, dictionary_gap), (compressed_size, value_count, codec), stats)| {
                ChunkModel {
                    data_page_offset,
                    dictionary_gap,
                    compressed_size,
                    value_count,
                    codec,
                    null_count: stats.0,
                    min: stats.1,
                    max: stats.2,
                }
            },
        )
}

/// Row groups of column chunks: every row group has the same number of columns. In
/// about half of them, as in most real files, a column's chunks have the same fields in
/// every row group.
fn row_groups_model() -> impl Strategy<Value = Vec<Vec<ChunkModel>>> {
    (0_usize..8, 0_usize..5, any::<bool>())
        .prop_flat_map(|(row_groups, columns, alike)| {
            let chunks = prop::collection::vec(chunk_model(), columns);
            (prop::collection::vec(chunks, row_groups), Just(alike))
        })
        .prop_map(|(mut row_groups, alike)| {
            if alike && let Some((first, rest)) = row_groups.split_first_mut() {
                for chunks in rest {
                    for (chunk, shape) in chunks.iter_mut().zip(first.iter()) {
                        *chunk = chunk.clone().shaped_like(shape);
                    }
                }
            }
            row_groups
        })
}

proptest! {
    #[test]
    fn generated_footers_extract_to_what_was_written(
        model in row_groups_model(),
        reversed in any::<bool>(),
        unknown_field in any::<bool>(),
    ) {
        let columns = model.first().map_or(0, Vec::len);
        let mut elements = vec![root(i32::try_from(columns).unwrap())];
        elements.extend((0..columns).map(|column| leaf(&format!("c{column}"), 1, 1)));
        let row_groups = model
            .iter()
            .map(|chunks| {
                let chunks = chunks.iter().map(|chunk| chunk.to_value(unknown_field)).collect();
                row_group(chunks, 3)
            })
            .collect();
        let mut fields = vec![
            (1, Value::I32(1)),
            (2, structs(elements)),
            (3, Value::I64(3 * i64::try_from(model.len()).unwrap())),
            (4, structs(row_groups)),
        ];
        if reversed {
            fields.reverse();
        }
        let (metadata, schema) = opened(fields);
        prop_assert_eq!(metadata.row_group_count() as usize, model.len());
        // With no row groups the schema alone says how many columns there are.
        if !model.is_empty() {
            prop_assert_eq!(schema.column_count() as usize, columns);
        }
        let mut facts = ChunkFacts::new();
        metadata.extract(&everything(&metadata), &mut facts).unwrap();
        for (row_group, chunks) in (0..).zip(&model) {
            for (column, expected) in (0..).zip(chunks) {
                let location = facts.location(slot(row_group, column)).unwrap();
                prop_assert_eq!(location.start(), expected.start());
                prop_assert_eq!(location.compressed_size(), u64::from(expected.compressed_size));
                prop_assert_eq!(location.value_count(), u64::from(expected.value_count));
                prop_assert_eq!(location.dictionary_page_offset().is_some(), expected.dictionary_gap.is_some());
                let statistics = facts.statistics(slot(row_group, column)).unwrap();
                prop_assert_eq!(statistics.null_count(), expected.null_count.map(u64::from));
                prop_assert_eq!(statistics.min(&metadata), expected.min.as_deref());
                prop_assert_eq!(statistics.max(&metadata), expected.max.as_deref());
            }
        }
        // One column alone gives the same facts as that column among all of them.
        for column in 0..metadata.column_count() {
            let mut alone = ChunkFacts::new();
            let only = [column];
            let request = Request::new(Columns::Indices(&only), 0..metadata.row_group_count())
                .with_location()
                .with_statistics();
            metadata.extract(&request, &mut alone).unwrap();
            for row_group in 0..metadata.row_group_count() {
                prop_assert_eq!(alone.location(slot(row_group, 0)), facts.location(slot(row_group, column)));
                prop_assert_eq!(alone.statistics(slot(row_group, 0)), facts.statistics(slot(row_group, column)));
            }
        }
    }

    #[test]
    fn chunks_are_found_to_end_where_parsing_ends_them(
        shape in chunk_model(),
        models in prop::collection::vec(
            (chunk_model(), prop::bool::weighted(0.8), any::<bool>()),
            4..10,
        ),
        damage in prop::option::of((any::<prop::sample::Index>(), any::<u8>())),
    ) {
        // One column's chunks, most of them with the same fields, and sometimes one
        // byte among them changed.
        let chunks: Vec<Value> = models
            .into_iter()
            .map(|(model, alike, unknown_field)| {
                let model = if alike { model.shaped_like(&shape) } else { model };
                model.to_value(unknown_field)
            })
            .collect();
        let (mut bytes, _) = laid_out(&chunks);
        if let Some((position, byte)) = damage {
            let damaged = position.index(bytes.len());
            bytes[damaged] = byte;
        }
        let mut templates = Templates::new(bytes.len(), u32::try_from(chunks.len()).unwrap());
        let mut position = 0;
        for _ in 0..chunks.len() {
            templates.row_group_begin();
            let found = templates.chunk_end(0, &bytes, position);
            prop_assert_eq!(found, parsed_end(&bytes, position));
            let Ok(end) = found else {
                break;
            };
            position = end;
        }
    }

    #[test]
    fn arbitrary_tails_never_panic(
        tail in prop::collection::vec(any::<u8>(), 0..300),
        extra in 0_u64..1000,
    ) {
        let file_size = tail.len() as u64 + extra;
        if let Ok(Opened::File { metadata, .. }) = open(&tail, file_size, &Limits::new(), None) {
            let _ = metadata.extract(&everything(&metadata), &mut ChunkFacts::new());
            for pair in metadata.key_value_metadata() {
                let _ = pair;
            }
        }
    }

    #[test]
    fn arbitrary_footers_never_panic(footer in prop::collection::vec(any::<u8>(), 0..300)) {
        if let Ok(Opened::File { metadata, schema }) = open_footer(&footer) {
            let _ = metadata.extract(&everything(&metadata), &mut ChunkFacts::new());
            for pair in metadata.key_value_metadata() {
                let _ = pair;
            }
            if let Some(schema) = schema {
                for index in 0..schema.node_count() {
                    let node = schema.node(index).unwrap();
                    drop((node.name(), node.path(), node.logical_type()));
                }
            }
        }
    }

    #[test]
    fn a_damaged_real_footer_never_panics(
        position in any::<prop::sample::Index>(),
        byte in any::<u8>(),
        cut in any::<prop::sample::Index>(),
    ) {
        // A real footer with one byte changed, and the same footer cut short.
        let footer = footer_bytes(simple_fields());
        let mut damaged = footer.clone();
        damaged[position.index(footer.len())] = byte;
        for candidate in [&damaged[..], &footer[..cut.index(footer.len())]] {
            if let Ok(Opened::File { metadata, .. }) = open_footer(candidate) {
                let _ = metadata.extract(&everything(&metadata), &mut ChunkFacts::new());
            }
        }
    }
}
