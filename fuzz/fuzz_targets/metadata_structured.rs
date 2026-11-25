//! Structure-aware Parquet footer fuzz target.
//!
//! Generates valid (and optionally corrupted) multi-row-group `FileMetaData` footers with
//! controlled per-column chunk shapes (uniform, alternating two-shape, variable-length
//! bursts, and extra unknown/long-delta fields) to directly stress:
//! - chunk template matching, the alternate template slot, variable-stretch tracking, and
//!   miss eviction (`src/metadata/template/`);
//! - all four `extract_mode` specializations and `ChunkFacts` reuse;
//! - shared-schema fast path and mismatch fallback;
//! - differential agreement with `arrow-rs` 60.1's `ParquetMetaDataReader`.

#![no_main]

mod common;

use common::synth::{ByteCursor, ThriftWriter};
use libfuzzer_sys::fuzz_target;
use parkay::Limits;
use parkay::metadata::{ChunkFacts, ChunkSlot, Columns, Opened, Request, open};
use parkay::thrift::{CompactReader, ElementType};
use parquet::file::metadata::ParquetMetaDataReader;

/// Per-column shape strategy across row groups to exercise all template code paths.
#[derive(Clone, Copy)]
enum ColumnPattern {
    /// Same fields every row group, only offsets/counts/varint widths change (template hits).
    Uniform,
    /// Alternates between two distinct shapes (exercises `Template::alternate`).
    Alternating,
    /// Varies binary/list lengths across row groups (exercises `VARIABLE_COUNT_MAX` and
    /// multi-variable stretch comparison).
    VariableBursts,
    /// Includes optional unknown/long-delta fields and varying subsets (exercises miss
    /// eviction and fallback parsing).
    Noisy,
}

#[derive(Clone, Copy)]
struct ColumnSpec {
    physical_type: i32,
    pattern: ColumnPattern,
}

fn emit_schema(
    cursor: &mut ByteCursor<'_>,
    out: &mut ThriftWriter,
    cols: &[ColumnSpec],
    nested: bool,
) {
    let elem_count = if nested {
        cols.len() + 2
    } else {
        cols.len() + 1
    };
    out.write_list_header(12, u32::try_from(elem_count).expect("small schema"));

    // Root SchemaElement.
    let mut last = 0_i16;
    out.write_field_header(&mut last, 4, 8, false);
    out.write_str("root");
    out.write_field_header(&mut last, 5, 5, false);
    let root_children = if nested { cols.len() - 1 } else { cols.len() };
    out.write_zigzag_i32(i32::try_from(root_children).expect("small schema"));
    out.write_stop();

    for (idx, spec) in cols.iter().enumerate() {
        if nested && idx == 0 {
            // Optional nested group holding the first two leaf columns.
            let mut glast = 0_i16;
            out.write_field_header(&mut glast, 3, 5, false);
            out.write_zigzag_i32(1); // OPTIONAL
            out.write_field_header(&mut glast, 4, 8, false);
            out.write_str("g0");
            out.write_field_header(&mut glast, 5, 5, false);
            out.write_zigzag_i32(2);
            out.write_stop();
        }
        let mut flast = 0_i16;
        out.write_field_header(&mut flast, 1, 5, false);
        out.write_zigzag_i32(spec.physical_type);
        if spec.physical_type == 7 {
            // FIXED_LEN_BYTE_ARRAY requires positive type_length (field 2).
            out.write_field_header(&mut flast, 2, 5, false);
            out.write_zigzag_i32(1 + i32::from(cursor.next_u8() & 0x0F));
        }
        out.write_field_header(&mut flast, 3, 5, false);
        out.write_zigzag_i32(i32::from(cursor.next_u8() % 3)); // REQUIRED / OPTIONAL / REPEATED
        out.write_field_header(&mut flast, 4, 8, false);
        let name = format!("c{idx}");
        out.write_str(&name);
        out.write_stop();
    }
}

fn emit_statistics(
    cursor: &mut ByteCursor<'_>,
    out: &mut ThriftWriter,
    physical_type: i32,
    vary_len: bool,
) {
    let mut last = 0_i16;
    // null_count (field 3)
    out.write_field_header(&mut last, 3, 6, false);
    out.write_zigzag_i64(i64::from(cursor.next_u16()));
    // distinct_count (field 4)
    if cursor.next_bool() {
        out.write_field_header(&mut last, 4, 6, false);
        out.write_zigzag_i64(i64::from(cursor.next_u16()));
    }
    // max_value (field 5) & min_value (field 6)
    let fixed_len = match physical_type {
        0 => Some(1),
        1 | 4 => Some(4),
        2 | 5 => Some(8),
        _ => None,
    };
    let stat_len = fixed_len.unwrap_or_else(|| {
        if vary_len {
            1 + cursor.range_usize(40)
        } else {
            4
        }
    });
    let max_bytes = vec![cursor.next_u8(); stat_len];
    let min_bytes = vec![cursor.next_u8(); stat_len];
    out.write_field_header(&mut last, 5, 8, false);
    out.write_binary(&max_bytes);
    out.write_field_header(&mut last, 6, 8, false);
    out.write_binary(&min_bytes);
    if cursor.next_bool() {
        out.write_field_header(&mut last, 7, if cursor.next_bool() { 1 } else { 2 }, false);
        out.write_field_header(&mut last, 8, if cursor.next_bool() { 1 } else { 2 }, false);
    }
    out.write_stop();
}

fn emit_chunk_kv(cursor: &mut ByteCursor<'_>, out: &mut ThriftWriter, vary_len: bool) {
    let kv_count = if vary_len {
        1 + cursor.range_usize(4)
    } else {
        1
    };
    out.write_list_header(12, u32::try_from(kv_count).expect("small kv"));
    for k in 0..kv_count {
        let mut klast = 0_i16;
        out.write_field_header(&mut klast, 1, 8, false);
        let klen = if vary_len {
            1 + cursor.range_usize(12)
        } else {
            2
        };
        let key = vec![b'k' + u8::try_from(k).expect("small"); klen];
        out.write_binary(&key);
        out.write_field_header(&mut klast, 2, 8, false);
        let vlen = if vary_len { cursor.range_usize(24) } else { 3 };
        let val = vec![b'v'; vlen];
        out.write_binary(&val);
        out.write_stop();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "structured encoder passes chunk shape knobs explicitly"
)]
fn emit_column_chunk(
    cursor: &mut ByteCursor<'_>,
    out: &mut ThriftWriter,
    col_index: usize,
    rg_index: usize,
    spec: ColumnSpec,
    nested: bool,
    file_size: u64,
    allow_non_arrow_extras: bool,
) {
    let shape_b = match spec.pattern {
        ColumnPattern::Uniform => false,
        ColumnPattern::Alternating => rg_index & 1 != 0,
        ColumnPattern::VariableBursts => true,
        ColumnPattern::Noisy => cursor.next_bool(),
    };
    let vary_len = matches!(
        spec.pattern,
        ColumnPattern::VariableBursts | ColumnPattern::Noisy
    );

    let mut chunk_last = 0_i16;
    // Field 2: file_offset
    let data_offset = 4 + (cursor.next_varied_u64() % (file_size / 4).max(1));
    let compressed_size = 1 + (cursor.next_varied_u64() % 10_000);
    let uncompressed_size = compressed_size + (cursor.next_varied_u64() % 10_000);
    out.write_field_header(&mut chunk_last, 2, 6, false);
    out.write_zigzag_i64(data_offset.cast_signed());

    // Field 3: ColumnMetaData
    out.write_field_header(&mut chunk_last, 3, 12, false);
    let mut meta_last = 0_i16;

    // 1: type
    out.write_field_header(&mut meta_last, 1, 5, false);
    out.write_zigzag_i32(spec.physical_type);

    // 2: encodings (list<i32>)
    out.write_field_header(&mut meta_last, 2, 9, false);
    let enc_count = if vary_len {
        1 + cursor.range_usize(4)
    } else if shape_b {
        3
    } else {
        2
    };
    out.write_list_header(5, u32::try_from(enc_count).expect("small list"));
    for i in 0..enc_count {
        // Valid Parquet encodings: PLAIN (0), RLE (3), BIT_PACKED (4), RLE_DICTIONARY (8).
        let enc = match (i + usize::from(cursor.next_u8())) & 0x03 {
            0 => 0,
            1 => 3,
            2 => 4,
            _ => 8,
        };
        out.write_zigzag_i32(enc);
    }

    // 3: path_in_schema (list<string>)
    out.write_field_header(&mut meta_last, 3, 9, false);
    if nested && col_index < 2 {
        out.write_list_header(8, 2);
        out.write_str("g0");
        out.write_str(&format!("c{col_index}"));
    } else {
        out.write_list_header(8, 1);
        out.write_str(&format!("c{col_index}"));
    }

    // 4: codec (0=UNCOMPRESSED, 1=SNAPPY, 2=GZIP, 6=ZSTD)
    let codec = match cursor.next_u8() & 0x03 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 6,
    };
    out.write_field_header(&mut meta_last, 4, 5, false);
    out.write_zigzag_i32(codec);

    // 5: num_values, 6: total_uncompressed_size, 7: total_compressed_size
    out.write_field_header(&mut meta_last, 5, 6, false);
    out.write_zigzag_i64(i64::from(cursor.next_u16()));
    out.write_field_header(&mut meta_last, 6, 6, false);
    out.write_zigzag_i64(uncompressed_size.cast_signed());
    out.write_field_header(&mut meta_last, 7, 6, false);
    out.write_zigzag_i64(compressed_size.cast_signed());

    // 8: optional chunk key_value_metadata (variable-length binaries -> stresses variable stretches!)
    if vary_len || shape_b {
        out.write_field_header(&mut meta_last, 8, 9, false);
        emit_chunk_kv(cursor, out, vary_len);
    }

    // 9: data_page_offset
    out.write_field_header(&mut meta_last, 9, 6, false);
    out.write_zigzag_i64(data_offset.cast_signed());

    // 11: optional dictionary_page_offset (must precede data_page_offset for valid location)
    if shape_b && data_offset > 8 {
        let dict_offset =
            data_offset - (1 + (cursor.next_varied_u64() % (data_offset - 4)).min(1024));
        out.write_field_header(&mut meta_last, 11, 6, false);
        out.write_zigzag_i64(dict_offset.cast_signed());
    }

    // 12: optional statistics (absent in shape A for Alternating -> two distinct chunk shapes!)
    if !matches!(spec.pattern, ColumnPattern::Alternating) || shape_b {
        out.write_field_header(&mut meta_last, 12, 12, false);
        emit_statistics(cursor, out, spec.physical_type, vary_len);
    }

    // 14 & 15: optional bloom_filter_offset / bloom_filter_length
    if shape_b {
        out.write_field_header(&mut meta_last, 14, 6, false);
        out.write_zigzag_i64((data_offset + compressed_size).cast_signed());
        out.write_field_header(&mut meta_last, 15, 5, false);
        out.write_zigzag_i32(64);
    }

    // Optional unknown long-delta field to exercise unknown-field skipping inside templates.
    if allow_non_arrow_extras && matches!(spec.pattern, ColumnPattern::Noisy) && cursor.next_bool()
    {
        out.write_field_header(&mut meta_last, 99, 8, true);
        out.write_binary(&[0xAB; 5]);
    }

    out.write_stop(); // end ColumnMetaData

    // Optional page index offsets on ColumnChunk (fields 4..=7) for alternating shapes.
    if shape_b {
        out.write_field_header(&mut chunk_last, 4, 6, false);
        out.write_zigzag_i64((data_offset + compressed_size + 64).cast_signed());
        out.write_field_header(&mut chunk_last, 5, 5, false);
        out.write_zigzag_i32(32);
    }

    out.write_stop(); // end ColumnChunk
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let mut cursor = ByteCursor::new(data);
    let col_count = 1 + cursor.range_usize(6);
    let rg_count = 1 + cursor.range_usize(14);
    let file_size = 1_u64 << 36;

    let mut cols = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let physical_type = i32::from(cursor.next_u8() % 8);
        let pattern = match cursor.next_u8() & 0x03 {
            0 => ColumnPattern::Uniform,
            1 => ColumnPattern::Alternating,
            2 => ColumnPattern::VariableBursts,
            _ => ColumnPattern::Noisy,
        };
        cols.push(ColumnSpec {
            physical_type,
            pattern,
        });
    }

    let mut out = ThriftWriter::new();
    let mut root_last = 0_i16;

    // 1: version (1 or 2)
    out.write_field_header(&mut root_last, 1, 5, false);
    out.write_zigzag_i32(1 + i32::from(cursor.next_u8() & 1));

    // 2: schema
    out.write_field_header(&mut root_last, 2, 9, false);
    let nested = cols.len() >= 2 && cursor.next_bool();
    emit_schema(&mut cursor, &mut out, &cols, nested);

    // 3: num_rows
    let total_rows = i64::from(cursor.next_u16());
    out.write_field_header(&mut root_last, 3, 6, false);
    out.write_zigzag_i64(total_rows);

    // 4: row_groups
    out.write_field_header(&mut root_last, 4, 9, false);
    out.write_list_header(12, u32::try_from(rg_count).expect("rg_count <= 14"));
    let allow_extras = cursor.next_u8().is_multiple_of(4);
    for rg in 0..rg_count {
        let mut rg_last = 0_i16;
        out.write_field_header(&mut rg_last, 1, 9, false);
        out.write_list_header(12, u32::try_from(col_count).expect("col_count <= 6"));
        for (col_idx, &spec) in cols.iter().enumerate() {
            emit_column_chunk(
                &mut cursor,
                &mut out,
                col_idx,
                rg,
                spec,
                nested,
                file_size,
                allow_extras,
            );
        }
        out.write_field_header(&mut rg_last, 2, 6, false);
        out.write_zigzag_i64(i64::from(cursor.next_u16()) + 1);
        out.write_field_header(&mut rg_last, 3, 6, false);
        out.write_zigzag_i64(total_rows / i64::try_from(rg_count).expect("rg_count > 0"));
        out.write_stop();
    }

    // 5: optional key_value_metadata
    if cursor.next_bool() {
        let kv_count = 1 + cursor.range_usize(3);
        out.write_field_header(&mut root_last, 5, 9, false);
        out.write_list_header(12, u32::try_from(kv_count).expect("small kv"));
        for i in 0..kv_count {
            let mut klast = 0_i16;
            out.write_field_header(&mut klast, 1, 8, false);
            out.write_str(&format!("k{i}"));
            if cursor.next_bool() {
                out.write_field_header(&mut klast, 2, 8, false);
                out.write_str(&format!("v{i}"));
            }
            out.write_stop();
        }
    }

    // 6: optional created_by
    if cursor.next_bool() {
        out.write_field_header(&mut root_last, 6, 8, false);
        out.write_str("parkay-fuzz");
    }

    // 7: optional column_orders
    if cursor.next_bool() {
        out.write_field_header(&mut root_last, 7, 9, false);
        out.write_list_header(12, u32::try_from(col_count).expect("small"));
        for _ in 0..col_count {
            let mut olast = 0_i16;
            out.write_field_header(&mut olast, 1, 12, false);
            out.write_stop();
            out.write_stop();
        }
    }

    out.write_stop();
    out.maybe_corrupt(&mut cursor);

    let footer = out.into_bytes();
    let Some(tail) = common::footer_tail(&footer) else {
        return;
    };
    let limits = Limits::new();
    let Ok(Opened::File { metadata, schema }) = open(&tail, file_size, &limits, None) else {
        return;
    };
    let schema = schema.expect("no schema supplied");

    // Extract with all 4 modes and assert internal consistency.
    let rg_u32 = metadata.row_group_count();
    let col_u32 = metadata.column_count();
    let mut full = ChunkFacts::new();
    let full_res = metadata.extract(
        &Request::new(Columns::All, 0..rg_u32)
            .with_location()
            .with_statistics(),
        &mut full,
    );
    if full_res.is_ok() {
        let mut partial = ChunkFacts::new();
        metadata
            .extract(
                &Request::new(Columns::All, 0..rg_u32).with_location(),
                &mut partial,
            )
            .expect("location-only succeeds");
        for rg in 0..rg_u32 {
            for col in 0..col_u32 {
                let slot = ChunkSlot {
                    row_group: rg,
                    column: col,
                };
                assert_eq!(partial.location(slot), full.location(slot));
                assert_eq!(partial.statistics(slot), None);
            }
        }
        metadata
            .extract(
                &Request::new(Columns::All, 0..rg_u32).with_statistics(),
                &mut partial,
            )
            .expect("statistics-only succeeds");
        for rg in 0..rg_u32 {
            for col in 0..col_u32 {
                let slot = ChunkSlot {
                    row_group: rg,
                    column: col,
                };
                assert_eq!(partial.location(slot), None);
                assert_eq!(partial.statistics(slot), full.statistics(slot));
            }
        }
    }

    // Re-open with own schema supplied and assert identical metadata.
    let Ok(Opened::File {
        metadata: again, ..
    }) = open(&tail, file_size, &limits, Some(&schema))
    else {
        panic!("valid footer must re-open with its own schema");
    };
    assert_eq!(again.version(), metadata.version());
    assert_eq!(again.row_count(), metadata.row_count());
    assert_eq!(again.column_count(), metadata.column_count());
    assert_eq!(again.row_group_count(), metadata.row_group_count());

    // Differential check against arrow-rs on well-typed ascending footers (same preconditions
    // as `metadata_arrow_diff`, since `maybe_corrupt` can alter wire types that arrow-rs
    // ignores while parkay skips them as unknown).
    if full_res.is_ok()
        && common::fields_ascend(&footer, ElementType::Struct)
        && common::shapes::fields_typed(&footer, common::shapes::FILE_META_DATA)
        && common::walk(&mut CompactReader::new(&footer, 0), ElementType::Struct, 0).is_some()
        && let Ok(arrow) = ParquetMetaDataReader::decode_metadata(&footer)
    {
        assert_eq!(metadata.version(), arrow.file_metadata().version());
        assert_eq!(
            metadata.row_count(),
            u64::try_from(arrow.file_metadata().num_rows()).expect("non-negative")
        );
        assert_eq!(metadata.row_group_count() as usize, arrow.num_row_groups());
        assert_eq!(
            metadata.column_count() as usize,
            arrow.file_metadata().schema_descr().num_columns()
        );
        for rg in 0..rg_u32 {
            let arrow_rg = arrow.row_group(rg as usize);
            for col in 0..col_u32 {
                let slot = ChunkSlot {
                    row_group: rg,
                    column: col,
                };
                let loc = full.location(slot).expect("location extracted");
                let arrow_col = arrow_rg.column(col as usize);
                assert_eq!(
                    loc.data_page_offset(),
                    u64::try_from(arrow_col.data_page_offset()).expect("non-negative")
                );
                assert_eq!(
                    loc.compressed_size(),
                    u64::try_from(arrow_col.compressed_size()).expect("non-negative")
                );
                assert_eq!(
                    loc.uncompressed_size(),
                    u64::try_from(arrow_col.uncompressed_size()).expect("non-negative")
                );
                assert_eq!(
                    loc.dictionary_page_offset(),
                    arrow_col
                        .dictionary_page_offset()
                        .map(|o| u64::try_from(o).expect("non-negative"))
                );
            }
        }
    }
});
