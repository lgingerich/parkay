//! Test support shared by the crate's unit tests: an encoder for the Thrift compact
//! protocol over a dynamic tree of values.
//!
//! The encoder follows the specification independently of the reader, so a round trip
//! checks one against the other. It writes whatever tree it is given, which lets tests
//! build inputs that real writers rarely produce but the protocol allows: fields out of
//! order, long-form field ids, unknown fields, and repeated fields.

use super::ElementType;

/// One Thrift value. A struct lists its fields as `(id, value)` in the order to write
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
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
    /// The wire type of this value.
    pub(crate) const fn element_type(&self) -> ElementType {
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

/// The 4-bit type code the protocol uses for `element_type`.
pub(crate) const fn type_code(element_type: ElementType) -> u8 {
    match element_type {
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
pub(crate) fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn put_zigzag(value: i64, out: &mut Vec<u8>) {
    put_varint(((value << 1) ^ (value >> 63)).cast_unsigned(), out);
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the short form is used only for sizes below 15"
)]
pub(crate) fn put_list_header(element_type: ElementType, size: usize, out: &mut Vec<u8>) {
    if size < 15 {
        out.push(((size as u8) << 4) | type_code(element_type));
    } else {
        out.push(0xF0 | type_code(element_type));
        put_varint(size as u64, out);
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the delta form is used only for deltas in 1..=15"
)]
pub(crate) fn encode(value: &Value, out: &mut Vec<u8>) {
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
        Value::List(element_type, items) | Value::Set(element_type, items) => {
            put_list_header(*element_type, items.len(), out);
            for item in items {
                encode(item, out);
            }
        }
        Value::Map(key_type, value_type, entries) => {
            put_varint(entries.len() as u64, out);
            if !entries.is_empty() {
                out.push((type_code(*key_type) << 4) | type_code(*value_type));
            }
            for (entry_key, entry_value) in entries {
                encode(entry_key, out);
                encode(entry_value, out);
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

/// The bytes `value` encodes to.
pub(crate) fn encoded(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode(value, &mut out);
    out
}
