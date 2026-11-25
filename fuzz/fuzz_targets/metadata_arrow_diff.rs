//! parkay's footer decoding against arrow-rs 60.1's: when both accept a footer, they
//! must agree on what it says.
//!
//! The input is a selector byte, which is ignored so the seeds are shared with the
//! other targets, and then a footer.
//!
//! The two readers are compared only where their rules are the same:
//! - Only footers whose field ids ascend in every structure are compared. parkay
//!   rejects a repeated field where arrow-rs keeps the last one, and arrow-rs needs the
//!   schema before the row groups; see `common::fields_ascend`.
//! - Only footers whose known fields all have the wire types the format gives them
//!   are compared. arrow-rs reads a field as the type it expects whatever its wire type
//!   is, so it can lose its place where parkay skips the field as unknown; see
//!   `common::shapes`.
//! - Only footers every value of which decodes are compared. Opening a file passes over
//!   column chunks without decoding them, so it accepts, say, an integer too long for
//!   its width inside a chunk; arrow-rs decodes the chunk and loses its place. See
//!   `common::walk`.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::Limits;
use parkay::metadata::{
    ChunkFacts, ChunkSlot, ColumnOrder, Columns, CompressionCodec, Encoding, LogicalType, Metadata,
    Opened, PhysicalType, Repetition, Request, Statistics, TimeUnit, open,
};
use parkay::thrift::{CompactReader, ElementType};
use parquet::basic::{
    ColumnOrder as ArrowColumnOrder, CompressionCodec as ArrowCompressionCodec, ConvertedType,
    Encoding as ArrowEncoding, LogicalType as ArrowLogicalType, Repetition as ArrowRepetition,
    TimeUnit as ArrowTimeUnit, Type as ArrowType,
};
use parquet::file::metadata::ParquetMetaDataReader;
use parquet::file::statistics::Statistics as ArrowStatistics;
use parquet::schema::types::Type as ArrowSchemaType;

fn arrow_type(physical_type: PhysicalType) -> ArrowType {
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

fn arrow_repetition(repetition: Repetition) -> ArrowRepetition {
    match repetition {
        Repetition::Required => ArrowRepetition::REQUIRED,
        Repetition::Optional => ArrowRepetition::OPTIONAL,
        Repetition::Repeated => ArrowRepetition::REPEATED,
    }
}

fn arrow_codec(codec: CompressionCodec) -> ArrowCompressionCodec {
    match codec {
        CompressionCodec::Uncompressed => ArrowCompressionCodec::UNCOMPRESSED,
        CompressionCodec::Snappy => ArrowCompressionCodec::SNAPPY,
        CompressionCodec::Gzip => ArrowCompressionCodec::GZIP,
        CompressionCodec::Lzo => ArrowCompressionCodec::LZO,
        CompressionCodec::Brotli => ArrowCompressionCodec::BROTLI,
        CompressionCodec::Lz4 => ArrowCompressionCodec::LZ4,
        CompressionCodec::Zstd => ArrowCompressionCodec::ZSTD,
        CompressionCodec::Lz4Raw => ArrowCompressionCodec::LZ4_RAW,
    }
}

#[expect(
    deprecated,
    reason = "PLAIN_DICTIONARY is deprecated in arrow-rs but still valid in Parquet v1 files"
)]
const ENCODINGS: [(Encoding, ArrowEncoding); 10] = [
    (Encoding::Plain, ArrowEncoding::PLAIN),
    (Encoding::PlainDictionary, ArrowEncoding::PLAIN_DICTIONARY),
    (Encoding::Rle, ArrowEncoding::RLE),
    (Encoding::BitPacked, ArrowEncoding::BIT_PACKED),
    (
        Encoding::DeltaBinaryPacked,
        ArrowEncoding::DELTA_BINARY_PACKED,
    ),
    (
        Encoding::DeltaLengthByteArray,
        ArrowEncoding::DELTA_LENGTH_BYTE_ARRAY,
    ),
    (Encoding::DeltaByteArray, ArrowEncoding::DELTA_BYTE_ARRAY),
    (Encoding::RleDictionary, ArrowEncoding::RLE_DICTIONARY),
    (Encoding::ByteStreamSplit, ArrowEncoding::BYTE_STREAM_SPLIT),
    (Encoding::Alp, ArrowEncoding::ALP),
];

fn map_column_order(order: ArrowColumnOrder) -> ColumnOrder {
    match order {
        ArrowColumnOrder::TYPE_DEFINED_ORDER(_) => ColumnOrder::TypeDefined,
        ArrowColumnOrder::IEEE_754_TOTAL_ORDER => ColumnOrder::Ieee754Total,
        ArrowColumnOrder::INT96_TIMESTAMP_ORDER => ColumnOrder::Int96Timestamp,
        ArrowColumnOrder::UNKNOWN | ArrowColumnOrder::UNDEFINED => ColumnOrder::Unrecognized,
    }
}

fn map_time_unit(unit: ArrowTimeUnit) -> TimeUnit {
    match unit {
        ArrowTimeUnit::MILLIS => TimeUnit::Milliseconds,
        ArrowTimeUnit::MICROS => TimeUnit::Microseconds,
        ArrowTimeUnit::NANOS => TimeUnit::Nanoseconds,
    }
}

fn map_logical_type(logical: &ArrowLogicalType) -> LogicalType {
    match logical {
        ArrowLogicalType::String => LogicalType::String,
        ArrowLogicalType::Map => LogicalType::Map,
        ArrowLogicalType::List => LogicalType::List,
        ArrowLogicalType::Enum => LogicalType::Enum,
        ArrowLogicalType::Decimal(decimal) => LogicalType::Decimal {
            scale: decimal.scale,
            precision: decimal.precision,
        },
        ArrowLogicalType::Date => LogicalType::Date,
        ArrowLogicalType::Time(time) => LogicalType::Time {
            adjusted_to_utc: time.is_adjusted_to_u_t_c,
            unit: map_time_unit(time.unit),
        },
        ArrowLogicalType::Timestamp(timestamp) => LogicalType::Timestamp {
            adjusted_to_utc: timestamp.is_adjusted_to_u_t_c,
            unit: map_time_unit(timestamp.unit),
        },
        ArrowLogicalType::Integer(int) => LogicalType::Integer {
            bit_width: u8::try_from(int.bit_width).expect("parkay rejected negative bit_width"),
            signed: int.is_signed,
        },
        ArrowLogicalType::Unknown => LogicalType::Unknown,
        ArrowLogicalType::Json => LogicalType::Json,
        ArrowLogicalType::Bson => LogicalType::Bson,
        ArrowLogicalType::Uuid => LogicalType::Uuid,
        ArrowLogicalType::Float16 => LogicalType::Float16,
        ArrowLogicalType::Variant(_) => LogicalType::Variant,
        ArrowLogicalType::Geometry(_) => LogicalType::Geometry,
        ArrowLogicalType::Geography(_) => LogicalType::Geography,
        ArrowLogicalType::File => LogicalType::File,
        ArrowLogicalType::_Unknown { .. } => LogicalType::Unrecognized,
    }
}

fn assert_logical_type_eq(ours: Option<LogicalType>, theirs: &ArrowSchemaType) {
    let info = theirs.get_basic_info();
    if let Some(logical) = info.logical_type_ref() {
        assert_eq!(ours, Some(map_logical_type(logical)));
        return;
    }
    let expected = match info.converted_type() {
        ConvertedType::NONE => None,
        ConvertedType::UTF8 => Some(LogicalType::String),
        ConvertedType::MAP => Some(LogicalType::Map),
        ConvertedType::MAP_KEY_VALUE => Some(LogicalType::MapKeyValue),
        ConvertedType::LIST => Some(LogicalType::List),
        ConvertedType::ENUM => Some(LogicalType::Enum),
        ConvertedType::DECIMAL => match theirs {
            ArrowSchemaType::PrimitiveType {
                scale, precision, ..
            } => Some(LogicalType::Decimal {
                scale: *scale,
                precision: *precision,
            }),
            // arrow-rs does not store scale or precision on group nodes.
            ArrowSchemaType::GroupType { .. } => {
                assert!(matches!(ours, Some(LogicalType::Decimal { .. })));
                return;
            }
        },
        ConvertedType::DATE => Some(LogicalType::Date),
        ConvertedType::TIME_MILLIS => Some(LogicalType::Time {
            adjusted_to_utc: true,
            unit: TimeUnit::Milliseconds,
        }),
        ConvertedType::TIME_MICROS => Some(LogicalType::Time {
            adjusted_to_utc: true,
            unit: TimeUnit::Microseconds,
        }),
        ConvertedType::TIMESTAMP_MILLIS => Some(LogicalType::Timestamp {
            adjusted_to_utc: true,
            unit: TimeUnit::Milliseconds,
        }),
        ConvertedType::TIMESTAMP_MICROS => Some(LogicalType::Timestamp {
            adjusted_to_utc: true,
            unit: TimeUnit::Microseconds,
        }),
        ConvertedType::UINT_8 => Some(LogicalType::Integer {
            bit_width: 8,
            signed: false,
        }),
        ConvertedType::UINT_16 => Some(LogicalType::Integer {
            bit_width: 16,
            signed: false,
        }),
        ConvertedType::UINT_32 => Some(LogicalType::Integer {
            bit_width: 32,
            signed: false,
        }),
        ConvertedType::UINT_64 => Some(LogicalType::Integer {
            bit_width: 64,
            signed: false,
        }),
        ConvertedType::INT_8 => Some(LogicalType::Integer {
            bit_width: 8,
            signed: true,
        }),
        ConvertedType::INT_16 => Some(LogicalType::Integer {
            bit_width: 16,
            signed: true,
        }),
        ConvertedType::INT_32 => Some(LogicalType::Integer {
            bit_width: 32,
            signed: true,
        }),
        ConvertedType::INT_64 => Some(LogicalType::Integer {
            bit_width: 64,
            signed: true,
        }),
        ConvertedType::JSON => Some(LogicalType::Json),
        ConvertedType::BSON => Some(LogicalType::Bson),
        ConvertedType::INTERVAL => Some(LogicalType::Interval),
    };
    assert_eq!(ours, expected);
}

fn collect_schema_nodes<'a>(node: &'a ArrowSchemaType, out: &mut Vec<&'a ArrowSchemaType>) {
    out.push(node);
    if let ArrowSchemaType::GroupType { fields, .. } = node {
        for field in fields {
            collect_schema_nodes(field, out);
        }
    }
}

fn assert_stat_bound_eq(
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
    physical_type: PhysicalType,
    context: &str,
) {
    match (ours, theirs) {
        (None, None) => {}
        (Some(ours), Some(theirs)) => match physical_type {
            // arrow-rs normalizes bool stats to 1 byte (`data[0] != 0`) and slices
            // fixed-width integer/float stats to their type width.
            PhysicalType::Boolean => {
                let Ok([theirs_byte]) = <[u8; 1]>::try_from(theirs) else {
                    panic!("arrow-rs boolean stat bound is not 1 byte ({context}): {theirs:?}");
                };
                assert_eq!(
                    ours.first().map(|&byte| u8::from(byte != 0)),
                    Some(theirs_byte),
                    "{context}"
                );
            }
            PhysicalType::Int32 | PhysicalType::Float => {
                assert_eq!(ours.get(..4), Some(theirs), "{context}");
            }
            PhysicalType::Int64 | PhysicalType::Double => {
                assert_eq!(ours.get(..8), Some(theirs), "{context}");
            }
            PhysicalType::Int96 | PhysicalType::ByteArray | PhysicalType::FixedLenByteArray => {
                assert_eq!(ours, theirs, "{context}");
            }
        },
        _ => panic!("stat bound presence mismatch ({context}): ours={ours:?}, theirs={theirs:?}"),
    }
}

fn assert_statistics_eq(
    ours: &Statistics,
    metadata: &Metadata,
    theirs: Option<&ArrowStatistics>,
    physical_type: PhysicalType,
    row_group: u32,
    column: u32,
) {
    let Some(theirs) = theirs else {
        assert_eq!(ours.null_count(), None);
        assert_eq!(ours.distinct_count(), None);
        assert_eq!(ours.nan_count(), None);
        assert_eq!(ours.min(metadata), None);
        assert_eq!(ours.max(metadata), None);
        assert!(!ours.bounds_are_deprecated());
        assert_eq!(ours.min_is_exact(), None);
        assert_eq!(ours.max_is_exact(), None);
        return;
    };
    assert_eq!(
        ours.null_count(),
        theirs.null_count_opt(),
        "row group {row_group}, column {column} null_count"
    );
    assert_eq!(
        ours.distinct_count(),
        theirs.distinct_count_opt(),
        "row group {row_group}, column {column} distinct_count"
    );
    // arrow-rs only preserves `nan_count` on Float, Double, and FixedLenByteArray.
    if matches!(
        physical_type,
        PhysicalType::Float | PhysicalType::Double | PhysicalType::FixedLenByteArray
    ) {
        assert_eq!(
            ours.nan_count(),
            theirs.nan_count_opt(),
            "row group {row_group}, column {column} nan_count"
        );
    }
    let ours_min = ours.min(metadata);
    let ours_max = ours.max(metadata);
    assert_stat_bound_eq(
        ours_min,
        theirs.min_bytes_opt(),
        physical_type,
        &format!("row group {row_group}, column {column} min"),
    );
    assert_stat_bound_eq(
        ours_max,
        theirs.max_bytes_opt(),
        physical_type,
        &format!("row group {row_group}, column {column} max"),
    );
    // arrow-rs marks `is_min_max_deprecated` true whenever `min_value` and `max_value`
    // are both absent, even when neither deprecated bound is present either.
    if ours_min.is_some() || ours_max.is_some() {
        assert_eq!(
            ours.bounds_are_deprecated(),
            theirs.is_min_max_deprecated(),
            "row group {row_group}, column {column} bounds_are_deprecated"
        );
    }
    // arrow-rs only reads `is_min_value_exact` / `is_max_value_exact` from
    // `ColumnMetaData` for byte-array columns, and reports `false` when the bound is absent.
    if matches!(
        physical_type,
        PhysicalType::ByteArray | PhysicalType::FixedLenByteArray
    ) {
        assert_eq!(
            ours_min.is_some() && ours.min_is_exact().unwrap_or(false),
            theirs.min_is_exact(),
            "row group {row_group}, column {column} min_is_exact"
        );
        assert_eq!(
            ours_max.is_some() && ours.max_is_exact().unwrap_or(false),
            theirs.max_is_exact(),
            "row group {row_group}, column {column} max_is_exact"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((_, footer)) = data.split_first() else {
        return;
    };
    let Some(tail) = common::footer_tail(footer) else {
        return;
    };
    // Cover the full non-negative `i64` offset + size range without tripping file-size checks.
    let file_size = u64::MAX;
    let Ok(Opened::File { metadata, schema }) = open(&tail, file_size, &Limits::new(), None) else {
        return;
    };
    if !common::fields_ascend(footer, ElementType::Struct)
        || !common::shapes::fields_typed(footer, common::shapes::FILE_META_DATA)
        || common::walk(&mut CompactReader::new(footer, 0), ElementType::Struct, 0).is_none()
    {
        return;
    }
    let Ok(arrow) = ParquetMetaDataReader::decode_metadata(footer) else {
        return;
    };
    let schema = schema.expect("no schema was supplied, so one was decoded");
    let arrow_file = arrow.file_metadata();

    // File-level metadata.
    assert_eq!(metadata.version(), arrow_file.version());
    assert_eq!(
        i64::try_from(metadata.row_count()).ok(),
        Some(arrow_file.num_rows())
    );
    assert_eq!(metadata.row_group_count() as usize, arrow.num_row_groups());
    assert_eq!(metadata.column_count(), schema.column_count());
    assert_eq!(metadata.created_by(), Ok(arrow_file.created_by()));

    let ours_kv: Vec<(&str, Option<&str>)> = metadata
        .key_value_metadata()
        .map(|pair| {
            let pair = pair.expect("arrow-rs decoded every key-value pair");
            (pair.key(), pair.value())
        })
        .collect();
    let theirs_kv: Vec<(&str, Option<&str>)> = arrow_file
        .key_value_metadata()
        .into_iter()
        .flatten()
        .map(|pair| (pair.key.as_str(), pair.value.as_deref()))
        .collect();
    assert_eq!(ours_kv, theirs_kv);

    // Parkay enforces column-order count == leaf column count and single-field union
    // structs, which arrow-rs accepts lazily; compare whenever parkay accepts them.
    if let Ok(orders_opt) = metadata.column_orders()
        && let Ok(ours_orders) = orders_opt
            .map(Iterator::collect::<Result<Vec<ColumnOrder>, _>>)
            .transpose()
    {
        let theirs_orders: Option<Vec<ColumnOrder>> = arrow_file.column_orders().map(|orders| {
            orders
                .iter()
                .map(|&order| map_column_order(order))
                .collect()
        });
        assert_eq!(ours_orders, theirs_orders);
    }

    // Full schema tree (all nodes in depth-first pre-order).
    let mut arrow_nodes = Vec::new();
    collect_schema_nodes(arrow_file.schema(), &mut arrow_nodes);
    assert_eq!(schema.node_count() as usize, arrow_nodes.len());
    for (index, theirs) in (0..).zip(&arrow_nodes) {
        let ours = schema.node(index).expect("the index is within the tree");
        let info = theirs.get_basic_info();
        assert_eq!(ours.name(), theirs.name());
        let theirs_repetition = info.has_repetition().then(|| info.repetition());
        assert_eq!(ours.repetition().map(arrow_repetition), theirs_repetition);
        match theirs {
            ArrowSchemaType::GroupType { fields, .. } => {
                assert_eq!(ours.child_count() as usize, fields.len());
                assert_eq!(ours.physical_type(), None);
            }
            ArrowSchemaType::PrimitiveType {
                physical_type,
                type_length,
                ..
            } => {
                assert_eq!(ours.child_count(), 0);
                assert_eq!(ours.physical_type().map(arrow_type), Some(*physical_type));
                assert_eq!(ours.type_length(), u32::try_from(*type_length).ok());
            }
        }
        // For an empty schema (root with no children), arrow-rs constructs the root from
        // `name` alone and drops `field_id`, `logical_type`, and `converted_type`.
        if index != 0 || ours.child_count() > 0 {
            let theirs_field_id = info.has_id().then(|| info.id());
            assert_eq!(ours.field_id(), theirs_field_id);
            assert_logical_type_eq(ours.logical_type(), theirs);
        }
    }

    // Leaf columns: path, physical type, and definition/repetition levels.
    let arrow_schema = arrow_file.schema_descr();
    assert_eq!(schema.column_count() as usize, arrow_schema.num_columns());
    for (index, theirs) in (0..).zip(arrow_schema.columns()) {
        let ours = schema.column(index).expect("the index is a leaf column");
        let physical_type = ours.physical_type().expect("a leaf has a physical type");
        assert_eq!(arrow_type(physical_type), theirs.physical_type());
        let theirs_path: Vec<&str> = theirs.path().parts().iter().map(String::as_str).collect();
        assert_eq!(ours.path(), theirs_path);
        assert_eq!(
            i16::from(ours.definition_level_max()),
            theirs.max_def_level()
        );
        assert_eq!(
            i16::from(ours.repetition_level_max()),
            theirs.max_rep_level()
        );
    }

    // Row groups and column chunks.
    // Extract location and statistics separately so a rejected field in one (e.g., negative
    // distinct_count in statistics) does not skip checking the other.
    let mut locations = ChunkFacts::new();
    let location_request =
        Request::new(Columns::All, 0..metadata.row_group_count()).with_location();
    let has_location = metadata.extract(&location_request, &mut locations).is_ok();

    let mut statistics = ChunkFacts::new();
    let statistics_request =
        Request::new(Columns::All, 0..metadata.row_group_count()).with_statistics();
    let has_statistics = metadata
        .extract(&statistics_request, &mut statistics)
        .is_ok();

    // The data page encodings read further into each chunk than its location does, so
    // they are extracted on their own for the same reason.
    let mut page_encodings = ChunkFacts::new();
    let page_encodings_request =
        Request::new(Columns::All, 0..metadata.row_group_count()).with_data_page_encodings();
    let has_page_encodings = metadata
        .extract(&page_encodings_request, &mut page_encodings)
        .is_ok();

    for (row_group, theirs) in (0..).zip(arrow.row_groups()) {
        assert_eq!(
            metadata
                .row_group_row_count(row_group)
                .and_then(|rows| i64::try_from(rows).ok()),
            Some(theirs.num_rows())
        );
        assert_eq!(
            metadata
                .row_group_byte_size(row_group)
                .and_then(|bytes| i64::try_from(bytes).ok()),
            Some(theirs.total_byte_size())
        );
        for (column, theirs) in (0..).zip(theirs.columns()) {
            let slot = ChunkSlot { row_group, column };
            if has_location {
                let ours = locations
                    .location(slot)
                    .expect("every chunk location was extracted");
                let ours_values = [
                    ours.value_count(),
                    ours.compressed_size(),
                    ours.uncompressed_size(),
                    ours.data_page_offset(),
                ]
                .map(|value| i64::try_from(value).ok());
                let theirs_values = [
                    theirs.num_values(),
                    theirs.compressed_size(),
                    theirs.uncompressed_size(),
                    theirs.data_page_offset(),
                ]
                .map(Some);
                assert_eq!(
                    ours_values, theirs_values,
                    "row group {row_group}, column {column}"
                );
                assert_eq!(
                    ours.dictionary_page_offset()
                        .map(|offset| i64::try_from(offset).ok()),
                    theirs.dictionary_page_offset().map(Some),
                    "row group {row_group}, column {column} dictionary_page_offset"
                );
                assert_eq!(
                    arrow_codec(ours.codec()),
                    theirs.compression_codec(),
                    "row group {row_group}, column {column} codec"
                );
                for (our_encoding, their_encoding) in ENCODINGS {
                    assert_eq!(
                        ours.encodings().contains(our_encoding),
                        theirs.encodings_mask().is_set(their_encoding),
                        "row group {row_group}, column {column} encoding {our_encoding:?}"
                    );
                }
            }
            if has_page_encodings {
                // arrow-rs keeps the encoding statistics as the set of data page encodings.
                let ours = page_encodings
                    .location(slot)
                    .expect("every chunk location was extracted")
                    .data_page_encodings();
                let theirs = theirs.page_encoding_stats_mask();
                assert_eq!(
                    ours.is_some(),
                    theirs.is_some(),
                    "row group {row_group}, column {column} has data page encodings"
                );
                if let (Some(ours), Some(theirs)) = (ours, theirs) {
                    for (our_encoding, their_encoding) in ENCODINGS {
                        assert_eq!(
                            ours.contains(our_encoding),
                            theirs.is_set(their_encoding),
                            "row group {row_group}, column {column} data page encoding {our_encoding:?}"
                        );
                    }
                }
            }
            if has_statistics {
                let ours = statistics
                    .statistics(slot)
                    .expect("every chunk statistics was extracted");
                let physical_type = schema
                    .column(column)
                    .expect("the index is a leaf column")
                    .physical_type()
                    .expect("a leaf has a physical type");
                assert_statistics_eq(
                    ours,
                    &metadata,
                    theirs.statistics(),
                    physical_type,
                    row_group,
                    column,
                );
            }
        }
    }
});
