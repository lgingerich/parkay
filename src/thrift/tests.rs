use std::fs;
use std::path::Path;

use proptest::prelude::*;

use super::{
    CompactReader, ElementType, Field, FieldCursor, FieldType, MAX_SKIP_DEPTH, ThriftError,
    VarintTarget,
};

// ---------------------------------------------------------------------------
// A test-only encoder and a generic decoder over a dynamic value tree. The encoder
// follows the specification independently of the reader, so round trips check one
// against the other.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    /// Stored as bits so NaN payloads compare exactly.
    Double(u64),
    Binary(Vec<u8>),
    List(ElementType, Vec<Self>),
    Set(ElementType, Vec<Self>),
    Map(ElementType, ElementType, Vec<(Self, Self)>),
    Struct(Vec<(i16, Self)>),
    Uuid([u8; 16]),
}

impl Value {
    const fn element_type(&self) -> ElementType {
        match self {
            Self::Bool(_) => ElementType::Bool,
            Self::I8(_) => ElementType::I8,
            Self::I16(_) => ElementType::I16,
            Self::I32(_) => ElementType::I32,
            Self::I64(_) => ElementType::I64,
            Self::Double(_) => ElementType::Double,
            Self::Binary(_) => ElementType::Binary,
            Self::List(..) => ElementType::List,
            Self::Set(..) => ElementType::Set,
            Self::Map(..) => ElementType::Map,
            Self::Struct(_) => ElementType::Struct,
            Self::Uuid(_) => ElementType::Uuid,
        }
    }
}

const fn type_code(ty: ElementType) -> u8 {
    match ty {
        ElementType::Bool => 1,
        ElementType::I8 => 3,
        ElementType::I16 => 4,
        ElementType::I32 => 5,
        ElementType::I64 => 6,
        ElementType::Double => 7,
        ElementType::Binary => 8,
        ElementType::List => 9,
        ElementType::Set => 10,
        ElementType::Map => 11,
        ElementType::Struct => 12,
        ElementType::Uuid => 13,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "each byte takes the low 7 bits, or the final value below 0x80"
)]
fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_zigzag(value: i64, out: &mut Vec<u8>) {
    put_varint(((value << 1) ^ (value >> 63)).cast_unsigned(), out);
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the short form is used only for sizes below 15"
)]
fn put_list_header(ty: ElementType, size: usize, out: &mut Vec<u8>) {
    if size < 15 {
        out.push(((size as u8) << 4) | type_code(ty));
    } else {
        out.push(0xF0 | type_code(ty));
        put_varint(size as u64, out);
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the delta form is used only for deltas in 1..=15"
)]
fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Bool(b) => out.push(if *b { 1 } else { 2 }),
        Value::I8(v) => out.push(v.cast_unsigned()),
        Value::I16(v) => put_zigzag(i64::from(*v), out),
        Value::I32(v) => put_zigzag(i64::from(*v), out),
        Value::I64(v) => put_zigzag(*v, out),
        Value::Double(bits) => out.extend_from_slice(&bits.to_le_bytes()),
        Value::Binary(bytes) => {
            put_varint(bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        Value::List(ty, items) | Value::Set(ty, items) => {
            put_list_header(*ty, items.len(), out);
            for item in items {
                encode(item, out);
            }
        }
        Value::Map(key, val, entries) => {
            put_varint(entries.len() as u64, out);
            if !entries.is_empty() {
                out.push((type_code(*key) << 4) | type_code(*val));
            }
            for (k, v) in entries {
                encode(k, out);
                encode(v, out);
            }
        }
        Value::Struct(fields) => {
            let mut last = 0_i16;
            for (id, field) in fields {
                let code = match field {
                    Value::Bool(true) => 1,
                    Value::Bool(false) => 2,
                    other => type_code(other.element_type()),
                };
                let delta = i32::from(*id) - i32::from(last);
                if (1..=15).contains(&delta) {
                    out.push(((delta as u8) << 4) | code);
                } else {
                    out.push(code);
                    put_zigzag(i64::from(*id), out);
                }
                if !matches!(field, Value::Bool(_)) {
                    encode(field, out);
                }
                last = *id;
            }
            out.push(0);
        }
        Value::Uuid(bytes) => out.extend_from_slice(bytes),
    }
}

fn decode(reader: &mut CompactReader<'_>, ty: ElementType) -> Result<Value, ThriftError> {
    Ok(match ty {
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
            if ty == ElementType::List {
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
                let value = match field.ty() {
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
    let size = CompactReader::new(&size_bytes).read_size(0)?;
    if size == 0 {
        return Ok(Value::Map(ElementType::Bool, ElementType::Bool, Vec::new()));
    }
    let types = reader.read_i8()?.cast_unsigned();
    let key = ElementType::from_code(types >> 4, 0)?;
    let val = ElementType::from_code(types & 0x0F, 0)?;
    let entries = (0..size)
        .map(|_| Ok((decode(reader, key)?, decode(reader, val)?)))
        .collect::<Result<_, ThriftError>>()?;
    Ok(Value::Map(key, val, entries))
}

fn field_to_element(field: Field) -> ElementType {
    super::field_element_type(field.ty()).expect("bool fields are handled by the caller")
}

fn encoded(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode(value, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Byte-level edge cases
// ---------------------------------------------------------------------------

fn reader(bytes: &[u8]) -> CompactReader<'_> {
    CompactReader::new(bytes)
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
        let mut r = reader(&bytes);
        assert_eq!(r.read_i64(), Ok(value));
        assert_eq!(r.position(), bytes.len());
    }
    assert_eq!(reader(&[0xFE, 0xFF, 0x03]).read_i16(), Ok(i16::MAX));
    assert_eq!(reader(&[0xFF, 0xFF, 0x03]).read_i16(), Ok(i16::MIN));
    assert_eq!(
        reader(&[0xFE, 0xFF, 0xFF, 0xFF, 0x0F]).read_i32(),
        Ok(i32::MAX)
    );
}

#[test]
fn padded_varint_within_width_is_accepted() {
    assert_eq!(reader(&[0x82, 0x80, 0x00]).read_i16(), Ok(1));
}

#[test]
fn varint_with_bits_beyond_width_is_rejected() {
    let overflow = |target| ThriftError::VarintOverflow { offset: 0, target };
    // Third byte of an i16 may hold only 2 bits.
    assert_eq!(
        reader(&[0xFF, 0xFF, 0x04]).read_i16(),
        Err(overflow(VarintTarget::I16))
    );
    // Fifth byte of an i32 may hold only 4 bits.
    assert_eq!(
        reader(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).read_i32(),
        Err(overflow(VarintTarget::I32))
    );
    // Tenth byte of an i64 may hold only 1 bit.
    let mut long = [0xFF; 10];
    long[9] = 0x02;
    assert_eq!(reader(&long).read_i64(), Err(overflow(VarintTarget::I64)));
}

#[test]
fn varint_longer_than_width_is_rejected() {
    let overflow = |target| ThriftError::VarintOverflow { offset: 0, target };
    assert_eq!(
        reader(&[0x80, 0x80, 0x80, 0x00]).read_i16(),
        Err(overflow(VarintTarget::I16))
    );
    assert_eq!(
        reader(&[0x80; 11]).read_i64(),
        Err(overflow(VarintTarget::I64))
    );
}

#[test]
fn input_ending_mid_value_is_an_error() {
    assert_eq!(
        reader(&[0x80]).read_i32(),
        Err(ThriftError::UnexpectedEof {
            offset: 1,
            needed: 1
        })
    );
    assert_eq!(
        reader(&[0; 5]).read_double(),
        Err(ThriftError::UnexpectedEof {
            offset: 0,
            needed: 8
        })
    );
    assert_eq!(
        reader(&[]).read_i8(),
        Err(ThriftError::UnexpectedEof {
            offset: 0,
            needed: 1
        })
    );
}

#[test]
fn bool_elements_accept_zero_and_two_as_false() {
    assert_eq!(reader(&[1]).read_bool_element(), Ok(true));
    assert_eq!(reader(&[2]).read_bool_element(), Ok(false));
    assert_eq!(reader(&[0]).read_bool_element(), Ok(false));
    assert_eq!(
        reader(&[3]).read_bool_element(),
        Err(ThriftError::InvalidBool { offset: 0, byte: 3 })
    );
}

#[test]
fn bool_fields_carry_their_value_in_the_header() {
    // Field 1 true, field 2 false, stop.
    let bytes = [0x11, 0x12, 0x00];
    let mut r = reader(&bytes);
    let mut cursor = FieldCursor::default();
    let first = cursor.next(&mut r).unwrap().unwrap();
    assert_eq!((first.id(), first.ty()), (1, FieldType::Bool(true)));
    r.skip_field(first.ty()).unwrap();
    let second = cursor.next(&mut r).unwrap().unwrap();
    assert_eq!((second.id(), second.ty()), (2, FieldType::Bool(false)));
    assert_eq!(cursor.next(&mut r), Ok(None));
    assert_eq!(r.position(), bytes.len());
}

#[test]
fn field_ids_decode_in_both_forms() {
    // Field 5 (delta form), field 3 (long form, zigzag 6), field 300 (long form).
    let bytes = [0x55, 0x00, 0x05, 0x06, 0x00, 0x05, 0xD8, 0x04, 0x00, 0x00];
    let mut r = reader(&bytes);
    let mut cursor = FieldCursor::default();
    let mut ids = Vec::new();
    while let Some(field) = cursor.next(&mut r).unwrap() {
        ids.push(field.id());
        r.skip_field(field.ty()).unwrap();
    }
    assert_eq!(ids, [5, 3, 300]);
    assert_eq!(r.position(), bytes.len());
}

#[test]
fn field_id_delta_overflow_is_rejected() {
    // Field i16::MAX in long form, then a delta of 1.
    let bytes = [0x05, 0xFE, 0xFF, 0x03, 0x00, 0x15];
    let mut r = reader(&bytes);
    let mut cursor = FieldCursor::default();
    let field = cursor.next(&mut r).unwrap().unwrap();
    assert_eq!(field.id(), i16::MAX);
    r.skip_field(field.ty()).unwrap();
    assert_eq!(
        cursor.next(&mut r),
        Err(ThriftError::FieldIdOverflow { offset: 5 })
    );
}

#[test]
fn invalid_type_codes_are_rejected() {
    assert_eq!(
        FieldCursor::default().next(&mut reader(&[0x1E])),
        Err(ThriftError::InvalidType {
            offset: 0,
            code: 14
        })
    );
    // 0 is the stop code for fields but never a valid element type.
    assert_eq!(
        reader(&[0x10]).read_list_header(),
        Err(ThriftError::InvalidType { offset: 0, code: 0 })
    );
}

#[test]
fn empty_list_with_type_zero_is_accepted() {
    // fastparquet writes empty lists as a single 0x00 byte (arrow-rs issue 8826).
    let header = reader(&[0x00]).read_list_header().unwrap();
    assert_eq!(header.size(), 0);
    // Inside a struct: field 1 is such a list, then stop.
    let bytes = [0x19, 0x00, 0x00];
    let mut r = reader(&bytes);
    r.skip(ElementType::Struct).unwrap();
    assert_eq!(r.position(), bytes.len());
}

#[test]
fn list_headers_decode_in_both_forms() {
    let mut bytes = vec![0xE5];
    bytes.extend([0; 14]);
    let short = reader(&bytes).read_list_header().unwrap();
    assert_eq!((short.element_type(), short.size()), (ElementType::I32, 14));
    bytes = vec![0xF5, 0x0F];
    bytes.extend([0; 15]);
    let long = reader(&bytes).read_list_header().unwrap();
    assert_eq!((long.element_type(), long.size()), (ElementType::I32, 15));
}

#[test]
fn sizes_larger_than_the_input_are_rejected() {
    assert_eq!(
        reader(&[0x05, 1, 2]).read_binary(),
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
        reader(&bytes).read_list_header(),
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
        reader(&[0x80, 0x80, 0x80, 0x80, 0x08]).read_binary(),
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
    let mut skipper = reader(&wide_varint);
    skipper.skip(ElementType::Struct).unwrap();
    assert_eq!(skipper.position(), wide_varint.len());
    assert_eq!(
        reader(&wide_varint[1..]).read_i16(),
        Err(ThriftError::VarintOverflow {
            offset: 0,
            target: VarintTarget::I16
        })
    );

    // A bool element that is not 0, 1, or 2.
    let mut bool_list = reader(&[0x11, 0x03]);
    bool_list.skip(ElementType::List).unwrap();
    assert_eq!(bool_list.position(), 2);

    // Field ids are not computed, so a delta past i16::MAX is not an error.
    let id_overflow = [0x05, 0xFE, 0xFF, 0x03, 0x00, 0x15, 0x00, 0x00];
    let mut ids = reader(&id_overflow);
    ids.skip(ElementType::Struct).unwrap();
    assert_eq!(ids.position(), id_overflow.len());
}

#[test]
fn skip_still_checks_structure() {
    assert_eq!(
        reader(&[0x1E]).skip(ElementType::Struct),
        Err(ThriftError::InvalidType {
            offset: 0,
            code: 14
        })
    );
    // A binary field claiming 5 bytes with 1 left.
    assert!(matches!(
        reader(&[0x18, 0x05, 0x00]).skip(ElementType::Struct),
        Err(ThriftError::SizeExceedsInput { .. })
    ));
    // A varint that never terminates.
    assert_eq!(
        reader(&[0x80, 0x80]).skip(ElementType::I64),
        Err(ThriftError::UnexpectedEof {
            offset: 2,
            needed: 1
        })
    );
}

#[test]
fn empty_map_has_no_type_byte() {
    let mut r = reader(&[0x00, 0xAA]);
    r.skip(ElementType::Map).unwrap();
    assert_eq!(r.position(), 1);
}

#[test]
fn skipping_deep_nesting_stops_at_the_limit() {
    let depth = usize::from(MAX_SKIP_DEPTH);
    // At the limit: `depth` nested lists, each holding one list, ending in an empty one.
    let mut ok = vec![0x19; depth - 1];
    ok.push(0x09);
    let mut r = reader(&ok);
    r.skip(ElementType::List).unwrap();
    assert_eq!(r.position(), ok.len());

    let mut deep = vec![0x19; depth];
    deep.push(0x09);
    assert_eq!(
        reader(&deep).skip(ElementType::List),
        Err(ThriftError::DepthExceeded {
            offset: u32::from(MAX_SKIP_DEPTH),
            limit: MAX_SKIP_DEPTH
        })
    );
}

#[test]
fn uuid_and_double_skip_fixed_widths() {
    let mut r = reader(&[0; 24]);
    r.skip(ElementType::Uuid).unwrap();
    r.skip(ElementType::Double).unwrap();
    assert_eq!(r.position(), 24);
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
    let ty = items.first().map_or(ElementType::I32, Value::element_type);
    let items = items
        .into_iter()
        .filter(|item| item.element_type() == ty)
        .collect();
    (ty, items)
}

fn value() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(6, 128, 20, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..20).prop_map(|items| {
                let (ty, items) = homogeneous(items);
                Value::List(ty, items)
            }),
            prop::collection::vec(inner.clone(), 0..5).prop_map(|items| {
                let (ty, items) = homogeneous(items);
                Value::Set(ty, items)
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
        let mut r = CompactReader::new(&bytes);
        prop_assert_eq!(decode(&mut r, value.element_type())?, value);
        prop_assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn skip_consumes_exactly_one_encoded_value(value in value()) {
        let bytes = encoded(&value);
        let mut r = CompactReader::new(&bytes);
        r.skip(value.element_type())?;
        prop_assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        for code in 1..=13 {
            let ty = ElementType::from_code(code, 0)?;
            let mut r = CompactReader::new(&bytes);
            if r.skip(ty).is_ok() {
                prop_assert!(r.position() <= bytes.len());
            }
            drop(decode(&mut CompactReader::new(&bytes), ty));
        }
    }
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
        let mut r = CompactReader::new(footer);
        r.skip(ElementType::Struct)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(r.position(), footer.len(), "{}", path.display());
        checked += 1;
    }
    // Guards against the directory being empty or the filter silently matching nothing.
    assert!(checked > 50, "only {checked} footers checked");
}
