//! Wire-level type definitions and compact-protocol type codes.

use super::errors::ThriftError;
use super::reader::invalid_type;

/// 4-bit type codes defined by the Thrift compact protocol.
pub(super) mod wire_type {
    pub(in crate::thrift) const STOP: u8 = 0;
    pub(in crate::thrift) const BOOL_TRUE: u8 = 1;
    pub(in crate::thrift) const BOOL_FALSE: u8 = 2;
    pub(in crate::thrift) const I8: u8 = 3;
    pub(in crate::thrift) const I16: u8 = 4;
    #[cfg(test)]
    pub(in crate::thrift) const I32: u8 = 5;
    pub(in crate::thrift) const I64: u8 = 6;
    pub(in crate::thrift) const DOUBLE: u8 = 7;
    pub(in crate::thrift) const BINARY: u8 = 8;
    pub(in crate::thrift) const LIST: u8 = 9;
    pub(in crate::thrift) const SET: u8 = 10;
    pub(in crate::thrift) const MAP: u8 = 11;
    pub(in crate::thrift) const STRUCT: u8 = 12;
    pub(in crate::thrift) const UUID: u8 = 13;
}

/// The type of a list, set, or map element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    /// A boolean, one byte per element.
    Bool,
    /// An 8-bit integer.
    I8,
    /// A 16-bit integer.
    I16,
    /// A 32-bit integer.
    I32,
    /// A 64-bit integer.
    I64,
    /// A 64-bit float.
    Double,
    /// A byte string, used for both binary and string values.
    Binary,
    /// A list.
    List,
    /// A set, encoded the same way as a list.
    Set,
    /// A map.
    Map,
    /// A struct.
    Struct,
    /// A 16-byte UUID.
    Uuid,
}

impl ElementType {
    /// Decodes a 4-bit element type code. Both bool codes are accepted, as the
    /// specification requires for compatibility with older writers.
    #[inline(always)]
    pub(super) fn from_code(code: u8, start: usize) -> Result<Self, ThriftError> {
        const TABLE: [Option<ElementType>; 16] = [
            None,
            Some(ElementType::Bool),
            Some(ElementType::Bool),
            Some(ElementType::I8),
            Some(ElementType::I16),
            Some(ElementType::I32),
            Some(ElementType::I64),
            Some(ElementType::Double),
            Some(ElementType::Binary),
            Some(ElementType::List),
            Some(ElementType::Set),
            Some(ElementType::Map),
            Some(ElementType::Struct),
            Some(ElementType::Uuid),
            None,
            None,
        ];
        TABLE[usize::from(code & 0x0F)].ok_or_else(|| invalid_type(start, code))
    }

    /// The fewest bytes one value of this type can occupy on the wire.
    pub(super) const fn encoded_len_min(self) -> u64 {
        // Keep the catch-all arm: listing all 10 1-byte variants turns `read_list_header`'s
        // branchless `csel` into a lookup-table load (`docs/perf/thrift-skip.md`).
        match self {
            Self::Double => 8,
            Self::Uuid => 16,
            _ => 1,
        }
    }
}

/// The type of a struct field. A bool field carries its value in the field header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// A boolean whose value was read from the field header; no bytes follow.
    Bool(bool),
    /// An 8-bit integer.
    I8,
    /// A 16-bit integer.
    I16,
    /// A 32-bit integer.
    I32,
    /// A 64-bit integer.
    I64,
    /// A 64-bit float.
    Double,
    /// A byte string, used for both binary and string values.
    Binary,
    /// A list.
    List,
    /// A set, encoded the same way as a list.
    Set,
    /// A map.
    Map,
    /// A struct.
    Struct,
    /// A 16-byte UUID.
    Uuid,
}

impl FieldType {
    /// The element type of the value that follows this field header, or `None` for a
    /// bool field whose value is carried in the header itself.
    #[must_use]
    pub const fn element_type(self) -> Option<ElementType> {
        Some(match self {
            Self::Bool(_) => return None,
            Self::I8 => ElementType::I8,
            Self::I16 => ElementType::I16,
            Self::I32 => ElementType::I32,
            Self::I64 => ElementType::I64,
            Self::Double => ElementType::Double,
            Self::Binary => ElementType::Binary,
            Self::List => ElementType::List,
            Self::Set => ElementType::Set,
            Self::Map => ElementType::Map,
            Self::Struct => ElementType::Struct,
            Self::Uuid => ElementType::Uuid,
        })
    }

    /// Decodes a 4-bit field type code other than the stop code.
    #[inline(always)]
    pub(super) fn from_code(code: u8, start: usize) -> Result<Self, ThriftError> {
        const TABLE: [Option<FieldType>; 16] = [
            None,
            Some(FieldType::Bool(true)),
            Some(FieldType::Bool(false)),
            Some(FieldType::I8),
            Some(FieldType::I16),
            Some(FieldType::I32),
            Some(FieldType::I64),
            Some(FieldType::Double),
            Some(FieldType::Binary),
            Some(FieldType::List),
            Some(FieldType::Set),
            Some(FieldType::Map),
            Some(FieldType::Struct),
            Some(FieldType::Uuid),
            None,
            None,
        ];
        TABLE[usize::from(code & 0x0F)].ok_or_else(|| invalid_type(start, code))
    }
}

/// A struct field header: the field id and the type of the value that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub(super) id: i16,
    pub(super) field_type: FieldType,
}

impl Field {
    /// The field id from the Thrift definition.
    #[must_use]
    pub const fn id(self) -> i16 {
        self.id
    }

    /// The type of the field's value.
    #[must_use]
    pub const fn field_type(self) -> FieldType {
        self.field_type
    }
}

/// A list or set header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListHeader {
    pub(super) element_type: ElementType,
    pub(super) size: u32,
}

impl ListHeader {
    /// The type of every element.
    ///
    /// For an empty list this may be a placeholder (see
    /// [`CompactReader::read_list_header`](super::CompactReader::read_list_header)), so
    /// callers checking the element type must skip the check when [`size`](Self::size) is
    /// zero.
    #[must_use]
    pub const fn element_type(self) -> ElementType {
        self.element_type
    }

    /// The number of elements. Checked to fit in the remaining input.
    #[must_use]
    pub const fn size(self) -> u32 {
        self.size
    }
}
