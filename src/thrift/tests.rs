use std::fs;
use std::path::Path;

use proptest::prelude::*;

use super::test_support::{Value, encoded, put_zigzag};
use super::{
    CompactReader, ElementType, Field, FieldCursor, FieldType, Marks, SKIP_DEPTH_MAX, ThriftError,
    VarintTarget,
};

// ---------------------------------------------------------------------------
// A generic decoder over the dynamic value tree of `test_support`, for round trips.
// ---------------------------------------------------------------------------

fn decode(reader: &mut CompactReader<'_>, element_type: ElementType) -> Result<Value, ThriftError> {
    Ok(match element_type {
        ElementType::Bool => Value::Bool(reader.read_bool_element()?),
        ElementType::I8 => Value::I8(reader.read_i8()?),
        ElementType::I16 => Value::I16(reader.read_i16()?),
        ElementType::I32 => Value::I32(reader.read_i32()?),
        ElementType::I64 => Value::I64(reader.read_i64()?),
        ElementType::Double => Value::Double(reader.read_double()?.to_bits()),
        ElementType::Binary => Value::Binary(reader.read_binary()?.to_vec()),
        ElementType::List | ElementType::Set => {
            let header = reader.read_list_header()?;
            let items = (0..header.size())
                .map(|_| decode(reader, header.element_type()))
                .collect::<Result<_, _>>()?;
            if element_type == ElementType::List {
                Value::List(header.element_type(), items)
            } else {
                Value::Set(header.element_type(), items)
            }
        }
        ElementType::Map => decode_map(reader)?,
        ElementType::Struct => {
            let mut fields = Vec::new();
            let mut cursor = FieldCursor::default();
            while let Some(field) = cursor.next(reader)? {
                let value = match field.field_type() {
                    FieldType::Bool(b) => Value::Bool(b),
                    _ => decode(reader, field_to_element(field))?,
                };
                fields.push((field.id(), value));
            }
            Value::Struct(fields)
        }
        ElementType::Uuid => {
            let mut bytes = [0; 16];
            for byte in &mut bytes {
                *byte = reader.read_i8()?.cast_unsigned();
            }
            Value::Uuid(bytes)
        }
    })
}

/// Maps are only ever skipped by the reader, so the test decoder parses the header
/// itself, following the specification.
fn decode_map(reader: &mut CompactReader<'_>) -> Result<Value, ThriftError> {
    let mut size_bytes = Vec::new();
    loop {
        let byte = reader.read_i8()?.cast_unsigned();
        size_bytes.push(byte);
        if byte & 0x80 == 0 {
            break;
        }
    }
    let size = CompactReader::new(&size_bytes, 0).read_size(0)?;
    if size == 0 {
        return Ok(Value::Map(ElementType::Bool, ElementType::Bool, Vec::new()));
    }
    let types = reader.read_i8()?.cast_unsigned();
    let key_type = ElementType::from_code(types >> 4, 0)?;
    let value_type = ElementType::from_code(types & 0x0F, 0)?;
    let entries = (0..size)
        .map(|_| Ok((decode(reader, key_type)?, decode(reader, value_type)?)))
        .collect::<Result<_, ThriftError>>()?;
    Ok(Value::Map(key_type, value_type, entries))
}

fn field_to_element(field: Field) -> ElementType {
    field
        .field_type()
        .element_type()
        .expect("bool fields are handled by the caller")
}

// ---------------------------------------------------------------------------
// Byte-level edge cases
// ---------------------------------------------------------------------------

fn reader_over(bytes: &[u8]) -> CompactReader<'_> {
    CompactReader::new(bytes, 0)
}

#[test]
fn varint_boundaries_decode_exactly() {
    for value in [
        0,
        1,
        -1,
        63,
        -64,
        64,
        -65,
        i64::from(i32::MAX),
        i64::MIN,
        i64::MAX,
    ] {
        let mut bytes = Vec::new();
        put_zigzag(value, &mut bytes);
        let mut reader = reader_over(&bytes);
        assert_eq!(reader.read_i64(), Ok(value));
        assert_eq!(reader.position(), bytes.len());
    }
    assert_eq!(reader_over(&[0xFE, 0xFF, 0x03]).read_i16(), Ok(i16::MAX));
    assert_eq!(reader_over(&[0xFF, 0xFF, 0x03]).read_i16(), Ok(i16::MIN));
    assert_eq!(
        reader_over(&[0xFE, 0xFF, 0xFF, 0xFF, 0x0F]).read_i32(),
        Ok(i32::MAX)
    );
}

#[test]
fn padded_varint_within_width_is_accepted() {
    assert_eq!(reader_over(&[0x82, 0x80, 0x00]).read_i16(), Ok(1));
}

#[test]
fn varint_with_bits_beyond_width_is_rejected() {
    let overflow = |target| ThriftError::VarintOverflow { offset: 0, target };
    // Third byte of an i16 may hold only 2 bits.
    assert_eq!(
        reader_over(&[0xFF, 0xFF, 0x04]).read_i16(),
        Err(overflow(VarintTarget::I16))
    );
    // Fifth byte of an i32 may hold only 4 bits.
    assert_eq!(
        reader_over(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).read_i32(),
        Err(overflow(VarintTarget::I32))
    );
    // Tenth byte of an i64 may hold only 1 bit.
    let mut long = [0xFF; 10];
    long[9] = 0x02;
    assert_eq!(
        reader_over(&long).read_i64(),
        Err(overflow(VarintTarget::I64))
    );
}

#[test]
fn varint_longer_than_width_is_rejected() {
    let overflow = |target| ThriftError::VarintOverflow { offset: 0, target };
    assert_eq!(
        reader_over(&[0x80, 0x80, 0x80, 0x00]).read_i16(),
        Err(overflow(VarintTarget::I16))
    );
    assert_eq!(
        reader_over(&[0x80; 11]).read_i64(),
        Err(overflow(VarintTarget::I64))
    );
}

#[test]
fn input_ending_mid_value_is_an_error() {
    assert_eq!(
        reader_over(&[0x80]).read_i32(),
        Err(ThriftError::UnexpectedEof {
            offset: 1,
            needed: 1
        })
    );
    assert_eq!(
        reader_over(&[0; 5]).read_double(),
        Err(ThriftError::UnexpectedEof {
            offset: 0,
            needed: 8
        })
    );
    assert_eq!(
        reader_over(&[]).read_i8(),
        Err(ThriftError::UnexpectedEof {
            offset: 0,
            needed: 1
        })
    );
}

#[test]
fn bool_elements_accept_zero_and_two_as_false() {
    assert_eq!(reader_over(&[1]).read_bool_element(), Ok(true));
    assert_eq!(reader_over(&[2]).read_bool_element(), Ok(false));
    assert_eq!(reader_over(&[0]).read_bool_element(), Ok(false));
    assert_eq!(
        reader_over(&[3]).read_bool_element(),
        Err(ThriftError::InvalidBool { offset: 0, byte: 3 })
    );
}

#[test]
fn bool_fields_carry_their_value_in_the_header() {
    // Field 1 true, field 2 false, stop.
    let bytes = [0x11, 0x12, 0x00];
    let mut reader = reader_over(&bytes);
    let mut cursor = FieldCursor::default();
    let first = cursor.next(&mut reader).unwrap().unwrap();
    assert_eq!((first.id(), first.field_type()), (1, FieldType::Bool(true)));
    reader.skip_field(first.field_type()).unwrap();
    let second = cursor.next(&mut reader).unwrap().unwrap();
    assert_eq!(
        (second.id(), second.field_type()),
        (2, FieldType::Bool(false))
    );
    assert_eq!(cursor.next(&mut reader), Ok(None));
    assert_eq!(reader.position(), bytes.len());
}

#[test]
fn field_ids_decode_in_both_forms() {
    // Field 5 (delta form), field 3 (long form, zigzag 6), field 300 (long form).
    let bytes = [0x55, 0x00, 0x05, 0x06, 0x00, 0x05, 0xD8, 0x04, 0x00, 0x00];
    let mut reader = reader_over(&bytes);
    let mut cursor = FieldCursor::default();
    let mut ids = Vec::new();
    while let Some(field) = cursor.next(&mut reader).unwrap() {
        ids.push(field.id());
        reader.skip_field(field.field_type()).unwrap();
    }
    assert_eq!(ids, [5, 3, 300]);
    assert_eq!(reader.position(), bytes.len());
}

#[test]
fn field_id_delta_overflow_is_rejected() {
    // Field i16::MAX in long form, then a delta of 1.
    let bytes = [0x05, 0xFE, 0xFF, 0x03, 0x00, 0x15];
    let mut reader = reader_over(&bytes);
    let mut cursor = FieldCursor::default();
    let field = cursor.next(&mut reader).unwrap().unwrap();
    assert_eq!(field.id(), i16::MAX);
    reader.skip_field(field.field_type()).unwrap();
    assert_eq!(
        cursor.next(&mut reader),
        Err(ThriftError::FieldIdOverflow { offset: 5 })
    );
}

#[test]
fn invalid_type_codes_are_rejected() {
    assert_eq!(
        FieldCursor::default().next(&mut reader_over(&[0x1E])),
        Err(ThriftError::InvalidType {
            offset: 0,
            code: 14
        })
    );
    // 0 is the stop code for fields but never a valid element type.
    assert_eq!(
        reader_over(&[0x10]).read_list_header(),
        Err(ThriftError::InvalidType { offset: 0, code: 0 })
    );
}

#[test]
fn empty_list_with_type_zero_is_accepted() {
    // fastparquet writes empty lists as a single 0x00 byte (arrow-rs issue 8826).
    let header = reader_over(&[0x00]).read_list_header().unwrap();
    assert_eq!(header.size(), 0);
    // Inside a struct: field 1 is such a list, then stop.
    let bytes = [0x19, 0x00, 0x00];
    let mut reader = reader_over(&bytes);
    reader.skip(ElementType::Struct).unwrap();
    assert_eq!(reader.position(), bytes.len());
}

#[test]
fn list_headers_decode_in_both_forms() {
    let mut bytes = vec![0xE5];
    bytes.extend([0; 14]);
    let short = reader_over(&bytes).read_list_header().unwrap();
    assert_eq!((short.element_type(), short.size()), (ElementType::I32, 14));
    bytes = vec![0xF5, 0x0F];
    bytes.extend([0; 15]);
    let long = reader_over(&bytes).read_list_header().unwrap();
    assert_eq!((long.element_type(), long.size()), (ElementType::I32, 15));
}

#[test]
fn sizes_larger_than_the_input_are_rejected() {
    assert_eq!(
        reader_over(&[0x05, 1, 2]).read_binary(),
        Err(ThriftError::SizeExceedsInput {
            offset: 0,
            size: 5,
            remaining: 2
        })
    );
    // Two doubles need 16 bytes; only 8 remain.
    let mut bytes = vec![0x27];
    bytes.extend([0; 8]);
    assert_eq!(
        reader_over(&bytes).read_list_header(),
        Err(ThriftError::SizeExceedsInput {
            offset: 0,
            size: 2,
            remaining: 8
        })
    );
}

#[test]
fn sizes_above_i32_max_are_rejected() {
    // 2^31 as a varint.
    assert_eq!(
        reader_over(&[0x80, 0x80, 0x80, 0x80, 0x08]).read_binary(),
        Err(ThriftError::SizeOverflow {
            offset: 0,
            size: 1 << 31
        })
    );
}

#[test]
fn skip_ignores_errors_in_values_it_discards() {
    // An i16 field whose varint overflows i16: reading rejects it, skipping does not.
    let wide_varint = [0x14, 0xFF, 0xFF, 0x04, 0x00];
    let mut skipper = reader_over(&wide_varint);
    skipper.skip(ElementType::Struct).unwrap();
    assert_eq!(skipper.position(), wide_varint.len());
    assert_eq!(
        reader_over(&wide_varint[1..]).read_i16(),
        Err(ThriftError::VarintOverflow {
            offset: 0,
            target: VarintTarget::I16
        })
    );

    // A bool element that is not 0, 1, or 2.
    let mut bool_list = reader_over(&[0x11, 0x03]);
    bool_list.skip(ElementType::List).unwrap();
    assert_eq!(bool_list.position(), 2);

    // Field ids are not computed, so a delta past i16::MAX is not an error.
    let id_overflow = [0x05, 0xFE, 0xFF, 0x03, 0x00, 0x15, 0x00, 0x00];
    let mut ids = reader_over(&id_overflow);
    ids.skip(ElementType::Struct).unwrap();
    assert_eq!(ids.position(), id_overflow.len());
}

#[test]
fn skip_still_checks_structure() {
    assert_eq!(
        reader_over(&[0x1E]).skip(ElementType::Struct),
        Err(ThriftError::InvalidType {
            offset: 0,
            code: 14
        })
    );
    // A binary field claiming 5 bytes with 1 left.
    assert!(matches!(
        reader_over(&[0x18, 0x05, 0x00]).skip(ElementType::Struct),
        Err(ThriftError::SizeExceedsInput { .. })
    ));
    // A varint that never terminates.
    assert_eq!(
        reader_over(&[0x80, 0x80]).skip(ElementType::I64),
        Err(ThriftError::UnexpectedEof {
            offset: 2,
            needed: 1
        })
    );
}

#[test]
fn empty_map_has_no_type_byte() {
    let mut reader = reader_over(&[0x00, 0xAA]);
    reader.skip(ElementType::Map).unwrap();
    assert_eq!(reader.position(), 1);
}

#[test]
fn skipping_deep_nesting_stops_at_the_limit() {
    let depth = usize::from(SKIP_DEPTH_MAX);
    // At the limit: `depth` nested lists, each holding one list, ending in an empty one.
    let mut ok = vec![0x19; depth - 1];
    ok.push(0x09);
    let mut reader = reader_over(&ok);
    reader.skip(ElementType::List).unwrap();
    assert_eq!(reader.position(), ok.len());

    let mut deep = vec![0x19; depth];
    deep.push(0x09);
    assert_eq!(
        reader_over(&deep).skip(ElementType::List),
        Err(ThriftError::DepthExceeded {
            offset: u32::from(SKIP_DEPTH_MAX),
            limit: SKIP_DEPTH_MAX
        })
    );
}

#[test]
fn uuid_and_double_skip_fixed_widths() {
    let mut reader = reader_over(&[0; 24]);
    reader.skip(ElementType::Uuid).unwrap();
    reader.skip(ElementType::Double).unwrap();
    assert_eq!(reader.position(), 24);
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<i8>().prop_map(Value::I8),
        any::<i16>().prop_map(Value::I16),
        any::<i32>().prop_map(Value::I32),
        any::<i64>().prop_map(Value::I64),
        any::<u64>().prop_map(Value::Double),
        prop::collection::vec(any::<u8>(), 0..40).prop_map(Value::Binary),
        any::<[u8; 16]>().prop_map(Value::Uuid),
    ]
}

/// Keeps only the items whose type matches the first, so the list is homogeneous.
fn homogeneous(items: Vec<Value>) -> (ElementType, Vec<Value>) {
    let element_type = items.first().map_or(ElementType::I32, Value::element_type);
    let items = items
        .into_iter()
        .filter(|item| item.element_type() == element_type)
        .collect();
    (element_type, items)
}

fn value() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(6, 128, 20, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..20).prop_map(|items| {
                let (element_type, items) = homogeneous(items);
                Value::List(element_type, items)
            }),
            prop::collection::vec(inner.clone(), 0..5).prop_map(|items| {
                let (element_type, items) = homogeneous(items);
                Value::Set(element_type, items)
            }),
            (
                prop::collection::vec(inner.clone(), 0..5),
                prop::collection::vec(inner.clone(), 0..5)
            )
                .prop_map(|(keys, vals)| {
                    let (key_ty, keys) = homogeneous(keys);
                    let (val_ty, vals) = homogeneous(vals);
                    let entries: Vec<_> = keys.into_iter().zip(vals).collect();
                    if entries.is_empty() {
                        Value::Map(ElementType::Bool, ElementType::Bool, entries)
                    } else {
                        Value::Map(key_ty, val_ty, entries)
                    }
                }),
            prop::collection::vec((any::<i16>(), inner), 0..10).prop_map(Value::Struct),
        ]
    })
}

proptest! {
    #[test]
    fn encoded_values_decode_back_unchanged(value in value()) {
        let bytes = encoded(&value);
        let mut reader = CompactReader::new(&bytes, 0);
        prop_assert_eq!(decode(&mut reader, value.element_type())?, value);
        prop_assert_eq!(reader.position(), bytes.len());
    }

    #[test]
    fn skip_consumes_exactly_one_encoded_value(value in value()) {
        let bytes = encoded(&value);
        let mut reader = CompactReader::new(&bytes, 0);
        reader.skip(value.element_type())?;
        prop_assert_eq!(reader.position(), bytes.len());
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        for code in 1..=13 {
            let element_type = ElementType::from_code(code, 0)?;
            let mut reader = CompactReader::new(&bytes, 0);
            if reader.skip(element_type).is_ok() {
                prop_assert!(reader.position() <= bytes.len());
            }
            drop(decode(&mut CompactReader::new(&bytes, 0), element_type));
        }
    }

    #[test]
    fn marking_accepts_what_skip_accepts_and_ends_where_it_ends(
        bytes in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        for code in 1..=13 {
            let element_type = ElementType::from_code(code, 0)?;
            let mut skipped = CompactReader::new(&bytes, 0);
            let skip = skipped.skip(element_type);
            let mut marked = CompactReader::new(&bytes, 0);
            let mut marks = Marks::new();
            let marking = marked.skip_marking(element_type, &mut marks);
            prop_assert_eq!(marking, skip);
            if skip.is_ok() {
                prop_assert_eq!(marked.position(), skipped.position());
                prop_assert_eq!(marks.mask().len(), skipped.position());
            } else {
                // A failed marking leaves the marks as they were.
                prop_assert!(marks.mask().is_empty());
                prop_assert!(marks.binaries().is_empty());
            }
        }
    }

    #[test]
    fn bytes_that_agree_on_the_marked_bits_skip_to_the_same_end(
        value in value(),
        noise in prop::collection::vec(any::<u8>(), 1..64),
    ) {
        let bytes = encoded(&value);
        let marks = marks_of(&value);
        // Keep every marked bit, and take every other bit from the noise.
        let other: Vec<u8> = bytes
            .iter()
            .zip(marks.mask())
            .zip(noise.iter().cycle())
            .map(|((byte, mask), noise)| (byte & mask) | (noise & !mask))
            .collect();
        let mut again = CompactReader::new(&other, 0);
        again.skip(value.element_type())?;
        prop_assert_eq!(again.position(), bytes.len());
    }

    #[test]
    fn marks_come_after_marks_already_there(first in value(), second in value()) {
        let mut marks = marks_of(&first);
        let first_size = marks.mask().len();
        let first_binaries = marks.binaries().len();
        let alone = marks_of(&second);
        let bytes = encoded(&second);
        CompactReader::new(&bytes, 0).skip_marking(second.element_type(), &mut marks)?;
        prop_assert_eq!(&marks.mask()[first_size..], alone.mask());
        // The second value's binaries are listed by where they are in the whole mask.
        let shifted: Vec<u32> = alone
            .binaries()
            .iter()
            .map(|binary| binary + u32::try_from(first_size).unwrap())
            .collect();
        prop_assert_eq!(&marks.binaries()[first_binaries..], &shifted[..]);
    }

    #[test]
    fn a_listed_binary_of_another_length_leaves_a_value_that_skips_to_its_end(
        inner in value(),
        choice in any::<prop::sample::Index>(),
        contents in prop::collection::vec(any::<u8>(), 0..200),
    ) {
        // A value with at least one binary in it.
        let value = Value::Struct(vec![(1, inner), (2, Value::Binary(vec![1, 2, 3]))]);
        let bytes = encoded(&value);
        let marks = marks_of(&value);
        let start = marks.binaries()[choice.index(marks.binaries().len())] as usize;
        // The binary as it is, and another of any length in its place.
        let mut reader = CompactReader::new(&bytes, start);
        reader.read_binary()?;
        let mut other = bytes[..start].to_vec();
        other.extend(encoded(&Value::Binary(contents)));
        other.extend(&bytes[reader.position()..]);
        let mut again = CompactReader::new(&other, 0);
        again.skip(value.element_type())?;
        prop_assert_eq!(again.position(), other.len());
    }

    #[test]
    fn an_integer_of_another_length_leaves_a_value_that_skips_to_its_end(
        inner in value(),
        choice in any::<prop::sample::Index>(),
        padding in 0_usize..12,
    ) {
        // A value with at least one integer in it.
        let value = Value::Struct(vec![(1, inner), (2, Value::I64(1 << 40))]);
        let bytes = encoded(&value);
        let marks = marks_of(&value);
        // Bytes that end an integer: marked as a varint, with no continuation bit.
        let ends: Vec<usize> = (0..bytes.len())
            .filter(|&index| marks.mask()[index] == Marks::VARINT && bytes[index] < 0x80)
            .collect();
        let end = ends[choice.index(ends.len())];
        // The same integer, longer: its last byte continues into bytes of padding.
        let mut other = bytes[..end].to_vec();
        other.push(bytes[end] | 0x80);
        other.extend(std::iter::repeat_n(0x80, padding));
        other.push(0x00);
        other.extend(&bytes[end + 1..]);
        let mut again = CompactReader::new(&other, 0);
        again.skip(value.element_type())?;
        prop_assert_eq!(again.position(), other.len());
    }
}

/// The marks of skipping `value`.
fn marks_of(value: &Value) -> Marks {
    let bytes = encoded(value);
    let mut marks = Marks::new();
    let mut reader = CompactReader::new(&bytes, 0);
    reader
        .skip_marking(value.element_type(), &mut marks)
        .unwrap();
    assert_eq!(reader.position(), bytes.len());
    assert_eq!(marks.mask().len(), bytes.len());
    marks
}

#[test]
fn marks_say_which_bits_decide_where_a_value_ends() {
    // A struct with an integer of two bytes, a binary of three, and a nested list of
    // two one-byte integers.
    let value = Value::Struct(vec![
        (1, Value::I32(300)),
        (2, Value::Binary(vec![7, 8, 9])),
        (
            3,
            Value::List(ElementType::I8, vec![Value::I8(1), Value::I8(2)]),
        ),
    ]);
    let marks = marks_of(&value);
    let expected = [
        0xFF, // field 1 header
        0x80, 0x80, // the integer's two bytes: only their continuation bits
        0xFF, // field 2 header
        0xFF, // the binary's length
        0x00, 0x00, 0x00, // the binary's contents
        0xFF, // field 3 header
        0xFF, // the list's header
        0x00, 0x00, // the list's two elements
        0xFF, // the stop marker
    ];
    assert_eq!(marks.mask(), expected);
    // The binary's length is the fifth byte.
    assert_eq!(marks.binaries(), [4]);
}

#[test]
fn marks_cover_long_binaries_and_padded_integers() {
    // Marks are written a word at a time into a mask that grows in steps, so cross
    // both a step and a word: a binary longer than a step, then an integer padded to
    // more bytes than a word, which skipping accepts.
    let mut bytes = encoded(&Value::Struct(vec![(1, Value::Binary(vec![0xAB; 700]))]));
    let stop = bytes.pop().unwrap();
    assert_eq!(stop, 0x00);
    bytes.push(0x16); // field 2, an i64
    bytes.extend([0x80; 11]);
    bytes.push(0x01);
    bytes.push(stop);
    let mut marks = Marks::new();
    let mut reader = CompactReader::new(&bytes, 0);
    reader
        .skip_marking(ElementType::Struct, &mut marks)
        .unwrap();
    assert_eq!(reader.position(), bytes.len());
    let mut expected = vec![0xFF, 0xFF, 0xFF]; // field 1 header and a two-byte length
    expected.extend([0x00; 700]);
    expected.push(0xFF); // field 2 header
    expected.extend([0x80; 12]);
    expected.push(0xFF); // the stop marker
    assert_eq!(marks.mask(), expected);
    assert_eq!(marks.binaries(), [1]);
}

// ---------------------------------------------------------------------------
// Real files
// ---------------------------------------------------------------------------

/// Returns the Thrift-encoded footer of an unencrypted Parquet file, or `None` for an
/// encrypted one (magic `PARE`).
fn footer(file: &[u8]) -> Option<&[u8]> {
    let (rest, magic) = file.split_last_chunk::<4>()?;
    if magic != b"PAR1" {
        return None;
    }
    let (rest, len) = rest.split_last_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    rest.get(rest.len().checked_sub(len)?..)
}

#[test]
fn every_parquet_testing_footer_skips_to_its_exact_end() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/parquet-testing/data");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).expect("run `git submodule update --init`") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "parquet") {
            continue;
        }
        let file = fs::read(&path).unwrap();
        let Some(footer) = footer(&file) else {
            continue;
        };
        let mut reader = CompactReader::new(footer, 0);
        reader
            .skip(ElementType::Struct)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(reader.position(), footer.len(), "{}", path.display());
        // Marking passes over the same bytes, with a mark for each.
        let mut marks = Marks::new();
        let mut marking = CompactReader::new(footer, 0);
        marking
            .skip_marking(ElementType::Struct, &mut marks)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(marking.position(), footer.len(), "{}", path.display());
        assert_eq!(marks.mask().len(), footer.len(), "{}", path.display());
        checked += 1;
    }
    // Guards against the directory being empty or the filter silently matching nothing.
    assert!(checked > 50, "only {checked} footers checked");
}
