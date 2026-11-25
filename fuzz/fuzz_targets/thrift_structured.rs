//! Structure-aware Thrift fuzz target.
//!
//! Generates valid (and optionally corrupted) deep/wide Thrift compact-protocol values
//! using [`common::synth`] so libFuzzer reaches paths raw byte mutations rarely hit:
//! - `List<Binary>` with >128 elements (crossing `BINARIES_GROWTH_COUNT_MIN` into
//!   `grow_bins` and `skip_list_tail_growing`);
//! - `List<I64>` with 1-byte, SWAR 2..5-byte, and >5-byte varints (`skip_varint_long_fast`);
//! - structs nested deeper than `INLINE_STACK = 4`, long-delta fields, maps, and nested
//!   lists (`skip_nested_elements_fast_cold`);
//! - exact parity between `skip`, `skip_marking` (including append/clear/rollback and
//!   mask soundness), `walk`, and `FieldCursor::next` + `skip_field`.

#![no_main]

mod common;

use common::synth::{ByteCursor, ThriftWriter};
use libfuzzer_sys::fuzz_target;
use parkay::thrift::{CompactReader, ElementType, FieldCursor, Marks, ThriftError};

/// Maximum generated recursion depth (well above `INLINE_STACK = 4` and well below
/// `SKIP_DEPTH_MAX = 64` unless intentionally pushed near the cap).
const MAX_DEPTH: u8 = 12;

/// Cap total emitted elements/fields per input so every run finishes in microseconds.
struct Budget {
    ops_left: usize,
}

impl Budget {
    const fn new(ops_left: usize) -> Self {
        Self { ops_left }
    }

    fn take(&mut self) -> bool {
        if self.ops_left == 0 {
            return false;
        }
        self.ops_left -= 1;
        true
    }
}

fn emit_binary(cursor: &mut ByteCursor<'_>, out: &mut ThriftWriter) {
    let len = match cursor.next_u8() & 0x07 {
        0 => 0,
        1..=5 => usize::from(cursor.next_u8() & 0x0F),
        6 => usize::from(cursor.next_u8() & 0x3F),
        // >127 bytes triggers multi-byte binary length (`read_size_fast_cold` / `read_binary_slow`).
        _ => 128 + usize::from(cursor.next_u8() & 0x3F),
    };
    let fill = cursor.next_u8();
    let payload = vec![fill; len];
    out.write_binary(&payload);
}

fn emit_value(
    cursor: &mut ByteCursor<'_>,
    out: &mut ThriftWriter,
    elem_code: u8,
    depth: u8,
    budget: &mut Budget,
) {
    match elem_code {
        1 | 2 => out.push_byte(u8::from(cursor.next_bool())),
        3 => out.push_byte(cursor.next_u8()),
        4 => out.write_zigzag_i16(cursor.next_u16().cast_signed()),
        5 => {
            let v = i32::from(cursor.next_u16().cast_signed()) << (cursor.next_u8() & 0x0F);
            out.write_zigzag_i32(v);
        }
        6 => out.write_zigzag_i64(cursor.next_varied_i64()),
        7 => {
            for _ in 0..8 {
                out.push_byte(cursor.next_u8());
            }
        }
        8 => emit_binary(cursor, out),
        9 | 10 => emit_list(cursor, out, depth, budget),
        11 => emit_map(cursor, out, depth, budget),
        12 => emit_struct(cursor, out, depth, budget),
        _ => {
            for _ in 0..16 {
                out.push_byte(cursor.next_u8());
            }
        }
    }
}

fn emit_list(cursor: &mut ByteCursor<'_>, out: &mut ThriftWriter, depth: u8, budget: &mut Budget) {
    let allow_nested = depth < MAX_DEPTH && budget.ops_left > 8;
    let elem_code = match cursor.next_u8() % if allow_nested { 11 } else { 7 } {
        0 => 2,  // Bool
        1 => 3,  // I8
        2 => 5,  // I32
        3 => 6,  // I64 (SWAR + long varint)
        4 => 7,  // Double
        5 => 8,  // Binary (window growth + bins growth)
        6 => 13, // Uuid
        7 => 12, // Struct (FlatLoop / iterative stack)
        8 => 9,  // List<List> (`skip_nested_elements_fast_cold`)
        9 => 10, // Set
        _ => 11, // Map
    };
    let max_len = match elem_code {
        // Large flat lists cross `MASK_GROW_LOOKAHEAD` and `BINARIES_GROWTH_COUNT_MIN = 128`.
        5 | 6 | 8 if depth <= 1 => 260,
        2 | 3 | 7 | 13 => 32,
        12 => 8,
        _ => 4,
    };
    let wanted = if max_len > 32 && cursor.next_u8().is_multiple_of(4) {
        // Frequently generate >130 elements to guarantee hitting `grow_bins` + `skip_list_tail_growing`.
        130 + cursor.range_usize(max_len - 130)
    } else {
        cursor.range_usize(max_len.min(budget.ops_left + 1))
    };
    let count = wanted.min(budget.ops_left);
    budget.ops_left -= count;
    let count_u32 = u32::try_from(count).expect("count <= 260");
    out.write_list_header(elem_code, count_u32);
    for _ in 0..count {
        emit_value(cursor, out, elem_code, depth + 1, budget);
    }
}

fn emit_map(cursor: &mut ByteCursor<'_>, out: &mut ThriftWriter, depth: u8, budget: &mut Budget) {
    let count = if depth >= MAX_DEPTH || budget.ops_left < 2 {
        0
    } else {
        cursor.range_usize(4).min(budget.ops_left / 2)
    };
    let key_code = match cursor.next_u8() & 0x03 {
        0 => 5, // I32
        1 => 6, // I64
        2 => 8, // Binary
        _ => 2, // Bool
    };
    let val_code = if depth + 1 < MAX_DEPTH {
        match cursor.next_u8() & 0x03 {
            0 => 8,  // Binary
            1 => 12, // Struct
            2 => 9,  // List
            _ => 6,  // I64
        }
    } else {
        6
    };
    let count_u32 = u32::try_from(count).expect("count < 4");
    out.write_map_header(key_code, val_code, count_u32);
    for _ in 0..count {
        if !budget.take() {
            break;
        }
        emit_value(cursor, out, key_code, depth + 1, budget);
        emit_value(cursor, out, val_code, depth + 1, budget);
    }
}

fn emit_struct(
    cursor: &mut ByteCursor<'_>,
    out: &mut ThriftWriter,
    depth: u8,
    budget: &mut Budget,
) {
    let field_count = cursor.range_usize(8);
    let mut last_id = 0_i16;
    for _ in 0..field_count {
        if !budget.take() {
            break;
        }
        let allow_nested = depth < MAX_DEPTH && budget.ops_left > 4;
        let wire_type = match cursor.next_u8() % if allow_nested { 13 } else { 9 } {
            0 => 1,   // Bool(true)
            1 => 2,   // Bool(false)
            2 => 3,   // I8
            3 => 4,   // I16
            4 => 5,   // I32
            5 => 6,   // I64
            6 => 7,   // Double
            7 => 8,   // Binary
            8 => 13,  // Uuid
            9 => 9,   // List
            10 => 10, // Set
            11 => 11, // Map
            _ => 12,  // Struct
        };
        let force_long = cursor.next_u8().is_multiple_of(16);
        let step = if force_long {
            i16::from(cursor.next_u8() & 0x3F)
        } else {
            1 + i16::from(cursor.next_u8() % 15)
        };
        let id = last_id.saturating_add(step);
        out.write_field_header(&mut last_id, id, wire_type, force_long);
        if wire_type >= 3 {
            emit_value(cursor, out, wire_type, depth + 1, budget);
        }
    }
    out.write_stop();
}

fn assert_skips_to_end(bytes: &[u8], element_type: ElementType, what: &str) {
    let mut reader = CompactReader::new(bytes, 0);
    assert_eq!(reader.skip(element_type), Ok(()), "{what}");
    assert_eq!(reader.position(), bytes.len(), "{what}");
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let mut cursor = ByteCursor::new(data);
    let mode = cursor.next_u8();
    let mut out = ThriftWriter::new();
    let mut budget = Budget::new(400);

    let element_type = match mode % 5 {
        0..=2 => {
            emit_struct(&mut cursor, &mut out, 0, &mut budget);
            ElementType::Struct
        }
        3 => {
            emit_list(&mut cursor, &mut out, 0, &mut budget);
            ElementType::List
        }
        _ => {
            emit_map(&mut cursor, &mut out, 0, &mut budget);
            ElementType::Map
        }
    };
    out.maybe_corrupt(&mut cursor);
    let input = out.into_bytes();

    // 1. Exact parity between `skip` and `skip_marking`.
    let mut plain = CompactReader::new(&input, 0);
    let plain_res = plain.skip(element_type);
    let mut marked = CompactReader::new(&input, 0);
    let mut marks = Marks::new();
    let mark_res = marked.skip_marking(element_type, &mut marks);
    assert_eq!(mark_res, plain_res, "skip and skip_marking disagree");

    // 2. Consistency with `walk` and `skip_field`.
    let mut walked_reader = CompactReader::new(&input, 0);
    let walked = common::walk(&mut walked_reader, element_type, 0);
    if walked.is_some() {
        assert_eq!(plain_res, Ok(()), "walk succeeded where skip failed");
        assert_eq!(plain.position(), walked_reader.position());
    }
    if element_type == ElementType::Struct {
        let mut by_field = CompactReader::new(&input, 0);
        let mut fields = FieldCursor::default();
        let field_res: Result<(), ThriftError> = (|| {
            while let Some(field) = fields.next(&mut by_field)? {
                by_field.skip_field(field.field_type())?;
            }
            Ok(())
        })();
        if field_res.is_ok() && !matches!(plain_res, Err(ThriftError::DepthExceeded { .. })) {
            assert_eq!(plain_res, Ok(()));
            assert_eq!(plain.position(), by_field.position());
        }
    }

    if plain_res.is_err() {
        assert_eq!(marks.mask(), []);
        assert_eq!(marks.binaries(), []);
        return;
    }

    let end = plain.position();
    assert_eq!(marked.position(), end);
    assert_eq!(marks.mask().len(), end);

    // 3. Reusing cleared `Marks` must produce identical marks.
    let saved_mask = marks.mask().to_vec();
    let saved_bins = marks.binaries().to_vec();
    marks.clear();
    let mut again = CompactReader::new(&input, 0);
    again
        .skip_marking(element_type, &mut marks)
        .expect("re-marking into cleared Marks succeeds");
    assert_eq!(marks.mask(), saved_mask.as_slice());
    assert_eq!(marks.binaries(), saved_bins.as_slice());

    // 4. Mask soundness: flipping every unmarked bit must preserve the skipped extent.
    let value = &input[..end];
    let flipped: Vec<u8> = value
        .iter()
        .zip(marks.mask())
        .map(|(&byte, &mask)| byte ^ !mask)
        .collect();
    assert_skips_to_end(&flipped, element_type, "unmarked bit flip changed end");
});
