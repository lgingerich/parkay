//! Decoding of the Thrift compact protocol, which Parquet uses for its footer and
//! page headers.
//!
//! This layer knows nothing about Parquet's own structs; those decoders are built on
//! top of it. It reads from a borrowed buffer, never allocates, and returns an error
//! on any malformed input.
//!
//! Every length the reader returns has been checked against the remaining input, so a
//! caller may allocate in proportion to it. The only unbounded structure an attacker
//! controls is the nesting of unknown fields, and [`CompactReader::skip`] limits that.
//!
//! This module imports nothing else from the crate, so it can move to its own crate
//! unchanged if that is ever needed.
//!
//! Reference: the Thrift compact protocol specification,
//! `doc/specs/thrift-compact-protocol.md` in the Apache Thrift repository.

#[cfg(test)]
mod tests;

use thiserror::Error;

/// Maximum container nesting that [`CompactReader::skip`] descends into.
///
/// Parquet's own structs nest only a few levels, so this bounds the recursion an
/// attacker can cause through unknown fields without rejecting real files.
pub const MAX_SKIP_DEPTH: u16 = 64;

/// Largest container size or binary length the protocol allows (the maximum `i32`).
const MAX_SIZE: u32 = i32::MAX.cast_unsigned();

/// A malformed Thrift compact-protocol input.
///
/// Every variant names the byte offset, within the buffer given to
/// [`CompactReader::new`], at which the bad value starts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ThriftError {
    /// The input ended before a value was complete.
    #[error("unexpected end of input at byte {offset}: needed {needed} more bytes")]
    UnexpectedEof {
        /// Offset of the first missing byte.
        offset: usize,
        /// Number of bytes the value needed from that offset.
        needed: usize,
    },
    /// A varint ran past the width of its target type.
    #[error("varint at byte {offset} overflows {target}")]
    VarintOverflow {
        /// Offset of the varint's first byte.
        offset: usize,
        /// The type being decoded, such as `i32`.
        target: &'static str,
    },
    /// A type code was not one the compact protocol defines in that position.
    #[error("invalid compact type code {code:#x} at byte {offset}")]
    InvalidType {
        /// Offset of the byte holding the code.
        offset: usize,
        /// The 4-bit type code found.
        code: u8,
    },
    /// A field id given as a delta from the previous one overflowed `i16`.
    #[error("field id overflows i16 at byte {offset}")]
    FieldIdOverflow {
        /// Offset of the field header.
        offset: usize,
    },
    /// A container size or binary length exceeded the protocol maximum of `i32::MAX`.
    #[error("size {size} at byte {offset} exceeds the protocol maximum")]
    SizeOverflow {
        /// Offset of the header holding the size.
        offset: usize,
        /// The size found.
        size: u32,
    },
    /// A container size or binary length claimed more data than the input holds.
    #[error("size {size} at byte {offset} cannot fit in the {remaining} bytes remaining")]
    SizeExceedsInput {
        /// Offset of the header holding the size.
        offset: usize,
        /// The size found: bytes for binary values, elements for containers.
        size: u32,
        /// Bytes left in the input after the header.
        remaining: usize,
    },
    /// A bool list element was not `1` (true) or `0`/`2` (false).
    #[error("invalid bool element {byte:#x} at byte {offset}")]
    InvalidBool {
        /// Offset of the byte.
        offset: usize,
        /// The byte found.
        byte: u8,
    },
    /// Containers being skipped nested deeper than [`MAX_SKIP_DEPTH`].
    #[error("containers nest deeper than {limit} at byte {offset}")]
    DepthExceeded {
        /// Offset of the container that crossed the limit.
        offset: usize,
        /// The limit, [`MAX_SKIP_DEPTH`].
        limit: u16,
    },
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
    fn from_code(code: u8, offset: usize) -> Result<Self, ThriftError> {
        Ok(match code {
            1 | 2 => Self::Bool,
            3 => Self::I8,
            4 => Self::I16,
            5 => Self::I32,
            6 => Self::I64,
            7 => Self::Double,
            8 => Self::Binary,
            9 => Self::List,
            10 => Self::Set,
            11 => Self::Map,
            12 => Self::Struct,
            13 => Self::Uuid,
            _ => return Err(ThriftError::InvalidType { offset, code }),
        })
    }

    /// The fewest bytes one value of this type can occupy on the wire.
    const fn min_encoded_len(self) -> u64 {
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
    /// Decodes a 4-bit field type code other than the stop code.
    fn from_code(code: u8, offset: usize) -> Result<Self, ThriftError> {
        Ok(match code {
            1 => Self::Bool(true),
            2 => Self::Bool(false),
            3 => Self::I8,
            4 => Self::I16,
            5 => Self::I32,
            6 => Self::I64,
            7 => Self::Double,
            8 => Self::Binary,
            9 => Self::List,
            10 => Self::Set,
            11 => Self::Map,
            12 => Self::Struct,
            13 => Self::Uuid,
            _ => return Err(ThriftError::InvalidType { offset, code }),
        })
    }
}

/// A struct field header: the field id and the type of the value that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    id: i16,
    ty: FieldType,
}

impl Field {
    /// The field id from the Thrift definition.
    #[must_use]
    pub const fn id(self) -> i16 {
        self.id
    }

    /// The type of the field's value.
    #[must_use]
    pub const fn ty(self) -> FieldType {
        self.ty
    }
}

/// A list or set header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListHeader {
    element_type: ElementType,
    size: u32,
}

impl ListHeader {
    /// The type of every element.
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

/// Walks the fields of one struct.
///
/// The caller holds the cursor, so nested structs need no stack inside the reader:
/// decoding a nested struct uses a fresh cursor.
#[derive(Debug, Default)]
pub struct FieldCursor {
    last_id: i16,
}

impl FieldCursor {
    /// Reads the next field header, or returns `None` at the struct's stop marker.
    ///
    /// After `None`, the struct is finished and the cursor must not be used again.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, the type code is invalid, or the field id
    /// overflows `i16`.
    pub fn next(&mut self, reader: &mut CompactReader<'_>) -> Result<Option<Field>, ThriftError> {
        let offset = reader.pos;
        let byte = reader.read_byte()?;
        // Like the reference implementations, only the low nibble decides the stop code.
        let code = byte & 0x0F;
        if code == 0 {
            return Ok(None);
        }
        let ty = FieldType::from_code(code, offset)?;
        let delta = byte >> 4;
        let id = if delta == 0 {
            reader.read_i16()?
        } else {
            self.last_id
                .checked_add(i16::from(delta))
                .ok_or(ThriftError::FieldIdOverflow { offset })?
        };
        self.last_id = id;
        Ok(Some(Field { id, ty }))
    }
}

/// Reads Thrift compact-protocol values from a borrowed buffer.
#[derive(Debug, Clone)]
pub struct CompactReader<'a> {
    input: &'a [u8],
    /// Offset of the next unread byte. Invariant: `pos <= input.len()`.
    pos: usize,
}

impl<'a> CompactReader<'a> {
    /// Creates a reader positioned at the start of `input`.
    #[must_use]
    pub const fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Offset of the next unread byte within the input.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Reads an `i8`, stored as one raw byte.
    ///
    /// # Errors
    ///
    /// Returns an error if the input has ended.
    pub fn read_i8(&mut self) -> Result<i8, ThriftError> {
        Ok(self.read_byte()?.cast_signed())
    }

    /// Reads an `i16`, stored as a zigzag varint.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends or the varint overflows `i16`.
    pub fn read_i16(&mut self) -> Result<i16, ThriftError> {
        let raw = self.read_varint(16, "i16")?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "read_varint bounds the value to 16 bits"
        )]
        let raw = raw as u16;
        Ok((raw >> 1).cast_signed() ^ (raw & 1).cast_signed().wrapping_neg())
    }

    /// Reads an `i32`, stored as a zigzag varint.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends or the varint overflows `i32`.
    pub fn read_i32(&mut self) -> Result<i32, ThriftError> {
        let raw = self.read_varint(32, "i32")?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "read_varint bounds the value to 32 bits"
        )]
        let raw = raw as u32;
        Ok((raw >> 1).cast_signed() ^ (raw & 1).cast_signed().wrapping_neg())
    }

    /// Reads an `i64`, stored as a zigzag varint.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends or the varint overflows `i64`.
    pub fn read_i64(&mut self) -> Result<i64, ThriftError> {
        let raw = self.read_varint(64, "i64")?;
        Ok((raw >> 1).cast_signed() ^ (raw & 1).cast_signed().wrapping_neg())
    }

    /// Reads a double, stored as 8 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the input has fewer than 8 bytes left.
    pub fn read_double(&mut self) -> Result<f64, ThriftError> {
        Ok(f64::from_le_bytes(*self.take_array::<8>()?))
    }

    /// Reads a binary or string value, borrowing it from the input.
    ///
    /// Strings are not checked for valid UTF-8 here; that is the caller's job.
    ///
    /// # Errors
    ///
    /// Returns an error if the length varint is malformed, exceeds `i32::MAX`, or
    /// claims more bytes than the input holds.
    pub fn read_binary(&mut self) -> Result<&'a [u8], ThriftError> {
        let offset = self.pos;
        let len = self.read_size(offset)?;
        self.check_fits(len, 1, offset)?;
        // `check_fits` proved `len <= remaining`, so the conversion cannot fail.
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.take(len)
    }

    /// Reads one bool list element: `1` is true, and both `0` and `2` are false.
    ///
    /// The specification says `2` for false; older versions of it said `0`, and some
    /// writers still emit that, so both are accepted.
    ///
    /// # Errors
    ///
    /// Returns an error if the input has ended or the byte is any other value.
    pub fn read_bool_element(&mut self) -> Result<bool, ThriftError> {
        let offset = self.pos;
        match self.read_byte()? {
            1 => Ok(true),
            0 | 2 => Ok(false),
            byte => Err(ThriftError::InvalidBool { offset, byte }),
        }
    }

    /// Reads a list or set header.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, the element type is invalid, or the size
    /// exceeds `i32::MAX` or cannot fit in the remaining input.
    pub fn read_list_header(&mut self) -> Result<ListHeader, ThriftError> {
        let offset = self.pos;
        let byte = self.read_byte()?;
        let element_type = ElementType::from_code(byte & 0x0F, offset)?;
        let short_size = byte >> 4;
        let size = if short_size == 0x0F {
            self.read_size(offset)?
        } else {
            u32::from(short_size)
        };
        self.check_fits(size, element_type.min_encoded_len(), offset)?;
        Ok(ListHeader { element_type, size })
    }

    /// Skips the value of a struct field. A bool field's value is in its header, so
    /// skipping it consumes nothing.
    ///
    /// # Errors
    ///
    /// Returns any error [`CompactReader::skip`] would.
    pub fn skip_field(&mut self, ty: FieldType) -> Result<(), ThriftError> {
        match field_element_type(ty) {
            Some(element_type) => self.skip(element_type),
            None => Ok(()),
        }
    }

    /// Skips one value of the given type, validating it as it goes.
    ///
    /// Containers are descended into at most [`MAX_SKIP_DEPTH`] levels deep.
    ///
    /// # Errors
    ///
    /// Returns an error if the value is malformed or nests too deeply.
    pub fn skip(&mut self, ty: ElementType) -> Result<(), ThriftError> {
        self.skip_nested(ty, 0)
    }

    fn skip_nested(&mut self, ty: ElementType, depth: u16) -> Result<(), ThriftError> {
        match ty {
            ElementType::Bool => {
                self.read_bool_element()?;
            }
            ElementType::I8 => {
                self.read_byte()?;
            }
            ElementType::I16 => {
                self.read_i16()?;
            }
            ElementType::I32 => {
                self.read_i32()?;
            }
            ElementType::I64 => {
                self.read_i64()?;
            }
            ElementType::Double => {
                self.take_array::<8>()?;
            }
            ElementType::Uuid => {
                self.take_array::<16>()?;
            }
            ElementType::Binary => {
                self.read_binary()?;
            }
            ElementType::List | ElementType::Set => {
                self.check_depth(depth)?;
                let header = self.read_list_header()?;
                for _ in 0..header.size {
                    self.skip_nested(header.element_type, depth + 1)?;
                }
            }
            ElementType::Map => {
                self.check_depth(depth)?;
                self.skip_map(depth)?;
            }
            ElementType::Struct => {
                self.check_depth(depth)?;
                let mut fields = FieldCursor::default();
                while let Some(field) = fields.next(self)? {
                    if let Some(element_type) = field_element_type(field.ty) {
                        self.skip_nested(element_type, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Skips a map: a size varint, then (if non-empty) one byte of key and value
    /// types, then the entries.
    fn skip_map(&mut self, depth: u16) -> Result<(), ThriftError> {
        let offset = self.pos;
        let size = self.read_size(offset)?;
        if size == 0 {
            return Ok(());
        }
        let types_offset = self.pos;
        let types = self.read_byte()?;
        let key = ElementType::from_code(types >> 4, types_offset)?;
        let value = ElementType::from_code(types & 0x0F, types_offset)?;
        self.check_fits(
            size,
            key.min_encoded_len() + value.min_encoded_len(),
            offset,
        )?;
        for _ in 0..size {
            self.skip_nested(key, depth + 1)?;
            self.skip_nested(value, depth + 1)?;
        }
        Ok(())
    }

    fn check_depth(&self, depth: u16) -> Result<(), ThriftError> {
        if depth >= MAX_SKIP_DEPTH {
            return Err(ThriftError::DepthExceeded {
                offset: self.pos,
                limit: MAX_SKIP_DEPTH,
            });
        }
        Ok(())
    }

    /// Reads a non-zigzag varint holding a container size or binary length.
    fn read_size(&mut self, offset: usize) -> Result<u32, ThriftError> {
        let raw = self.read_varint(32, "u32")?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "read_varint bounds the value to 32 bits"
        )]
        let size = raw as u32;
        if size > MAX_SIZE {
            return Err(ThriftError::SizeOverflow { offset, size });
        }
        Ok(size)
    }

    /// Checks that `size` values of at least `min_len` bytes each fit in the rest of
    /// the input.
    fn check_fits(&self, size: u32, min_len: u64, offset: usize) -> Result<(), ThriftError> {
        let remaining = self.input.len() - self.pos;
        // `min_len` is at most 32, so this cannot overflow.
        let needed = u64::from(size) * min_len;
        if needed > u64::try_from(remaining).unwrap_or(u64::MAX) {
            return Err(ThriftError::SizeExceedsInput {
                offset,
                size,
                remaining,
            });
        }
        Ok(())
    }

    /// Reads an unsigned LEB128 varint whose value must fit in `bits` bits.
    ///
    /// Padded encodings (extra `0x80` continuation bytes) are accepted, as the
    /// reference implementations do, as long as they stay within the byte limit for
    /// the width. Bits beyond the width are rejected rather than discarded.
    fn read_varint(&mut self, bits: u32, target: &'static str) -> Result<u64, ThriftError> {
        let offset = self.pos;
        let mut value = 0_u64;
        let mut shift = 0_u32;
        loop {
            let byte = self.read_byte()?;
            let payload = u64::from(byte & 0x7F);
            let room = bits - shift;
            if room < 7 && payload >> room != 0 {
                return Err(ThriftError::VarintOverflow { offset, target });
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= bits {
                return Err(ThriftError::VarintOverflow { offset, target });
            }
        }
    }

    fn read_byte(&mut self) -> Result<u8, ThriftError> {
        let byte = *self.input.get(self.pos).ok_or(ThriftError::UnexpectedEof {
            offset: self.pos,
            needed: 1,
        })?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ThriftError> {
        let bytes = self.input[self.pos..]
            .get(..len)
            .ok_or(ThriftError::UnexpectedEof {
                offset: self.pos,
                needed: len,
            })?;
        self.pos += len;
        Ok(bytes)
    }

    fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N], ThriftError> {
        let bytes =
            self.input[self.pos..]
                .first_chunk::<N>()
                .ok_or(ThriftError::UnexpectedEof {
                    offset: self.pos,
                    needed: N,
                })?;
        self.pos += N;
        Ok(bytes)
    }
}

/// The element type to skip for a field's value, or `None` for a bool field, whose
/// value is already in its header.
const fn field_element_type(ty: FieldType) -> Option<ElementType> {
    Some(match ty {
        FieldType::Bool(_) => return None,
        FieldType::I8 => ElementType::I8,
        FieldType::I16 => ElementType::I16,
        FieldType::I32 => ElementType::I32,
        FieldType::I64 => ElementType::I64,
        FieldType::Double => ElementType::Double,
        FieldType::Binary => ElementType::Binary,
        FieldType::List => ElementType::List,
        FieldType::Set => ElementType::Set,
        FieldType::Map => ElementType::Map,
        FieldType::Struct => ElementType::Struct,
        FieldType::Uuid => ElementType::Uuid,
    })
}
