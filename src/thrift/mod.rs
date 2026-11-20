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

mod errors;
#[cfg(test)]
mod tests;

pub use errors::{ThriftError, VarintTarget};

/// Maximum container nesting that [`CompactReader::skip`] descends into.
///
/// Parquet's own structs nest only a few levels, so this bounds the recursion an
/// attacker can cause through unknown fields without rejecting real files.
pub const MAX_SKIP_DEPTH: u16 = 64;

/// Largest container size or binary length the protocol allows (the maximum `i32`).
const MAX_SIZE: u32 = i32::MAX.cast_unsigned();

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
    fn from_code(code: u8, offset: u32) -> Result<Self, ThriftError> {
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
    fn from_code(code: u8, offset: u32) -> Result<Self, ThriftError> {
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
    ///
    /// For an empty list this may be a placeholder (see
    /// [`CompactReader::read_list_header`]), so callers checking the element type must
    /// skip the check when [`size`](Self::size) is zero.
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
        let offset = reader.offset();
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
        let raw = self.read_varint::<16>(VarintTarget::I16)?;
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
        let raw = self.read_varint::<32>(VarintTarget::I32)?;
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
        let raw = self.read_varint::<64>(VarintTarget::I64)?;
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
        let offset = self.offset();
        let size = self.read_size(offset)?;
        let rest = &self.input[self.pos..];
        let bytes = usize::try_from(size)
            .ok()
            .and_then(|len| rest.get(..len))
            .ok_or_else(|| ThriftError::SizeExceedsInput {
                offset,
                size,
                remaining: saturate_u32(rest.len()),
            })?;
        self.pos += bytes.len();
        Ok(bytes)
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
        let offset = self.offset();
        match self.read_byte()? {
            1 => Ok(true),
            0 | 2 => Ok(false),
            byte => Err(ThriftError::InvalidBool { offset, byte }),
        }
    }

    /// Reads a list or set header.
    ///
    /// A header byte of `0x00` (size 0, element type 0) is accepted as an empty list,
    /// with [`ElementType::I8`] as a placeholder element type. Type 0 is not a valid
    /// element type in the specification, but fastparquet writes empty lists this way
    /// and other readers tolerate it. See arrow-rs
    /// <https://github.com/apache/arrow-rs/issues/8826> and its fix,
    /// <https://github.com/apache/arrow-rs/blob/6fc14b60d76cd18e0c89c26a8643e2c5b370c185/parquet/src/parquet_thrift.rs#L305-L312>.
    /// A non-zero size with type 0 is still an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, the element type is invalid, or the size
    /// exceeds `i32::MAX` or cannot fit in the remaining input.
    // Benchmarked: faster than `#[inline]` in skip.
    #[inline(always)]
    pub fn read_list_header(&mut self) -> Result<ListHeader, ThriftError> {
        let offset = self.offset();
        let byte = self.read_byte()?;
        if byte == 0 {
            return Ok(ListHeader {
                element_type: ElementType::I8,
                size: 0,
            });
        }
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

    /// Skips one value of the given type.
    ///
    /// Skipping checks structure, not values. Type codes, container and binary sizes,
    /// and nesting depth are validated, because they decide where the value ends.
    /// Values that are thrown away are not: integers are not checked against their
    /// width, bool elements not checked for `0`/`1`/`2`, and field ids not computed.
    /// A value skipped here can still fail when read with the `read_*` methods.
    ///
    /// Containers are descended into at most [`MAX_SKIP_DEPTH`] levels deep.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, a type code or size is invalid, or
    /// containers nest too deeply.
    pub fn skip(&mut self, ty: ElementType) -> Result<(), ThriftError> {
        self.skip_nested(ty, 0)
    }

    fn skip_nested(&mut self, ty: ElementType, depth: u16) -> Result<(), ThriftError> {
        match ty {
            ElementType::List | ElementType::Set => {
                self.check_depth(depth)?;
                let header = self.read_list_header()?;
                let size = header.size;
                // Dispatch on the element type once, not per element.
                match header.element_type {
                    ElementType::Bool | ElementType::I8 => {
                        for _ in 0..size {
                            self.take_array::<1>()?;
                        }
                    }
                    ElementType::I16 | ElementType::I32 | ElementType::I64 => {
                        for _ in 0..size {
                            self.skip_varint()?;
                        }
                    }
                    ElementType::Double => {
                        for _ in 0..size {
                            self.take_array::<8>()?;
                        }
                    }
                    ElementType::Uuid => {
                        for _ in 0..size {
                            self.take_array::<16>()?;
                        }
                    }
                    ElementType::Binary => {
                        for _ in 0..size {
                            self.skip_binary()?;
                        }
                    }
                    container => {
                        for _ in 0..size {
                            self.skip_nested(container, depth + 1)?;
                        }
                    }
                }
                Ok(())
            }
            ElementType::Map => {
                self.check_depth(depth)?;
                self.skip_map(depth)
            }
            ElementType::Struct => {
                self.check_depth(depth)?;
                self.skip_struct_fields(depth)
            }
            ElementType::Bool | ElementType::I8 => self.take_array::<1>().map(drop),
            ElementType::I16 | ElementType::I32 | ElementType::I64 => self.skip_varint(),
            ElementType::Double => self.take_array::<8>().map(drop),
            ElementType::Uuid => self.take_array::<16>().map(drop),
            ElementType::Binary => self.skip_binary(),
        }
    }

    /// Skips struct fields up to and including the stop marker. Like
    /// [`FieldCursor::next`], but field ids are not needed, so a long-form id is
    /// skipped instead of decoded. Scalar fields are skipped inline, dispatching once
    /// on the raw type code; only containers recurse.
    fn skip_struct_fields(&mut self, depth: u16) -> Result<(), ThriftError> {
        // A local copy keeps the position in a register (benchmarked); `self` is synced
        // only around recursive calls.
        let mut r = self.clone();
        loop {
            let offset = r.offset();
            let byte = r.read_byte()?;
            let code = byte & 0x0F;
            if code == 0 {
                self.pos = r.pos;
                return Ok(());
            }
            if byte >> 4 == 0 {
                r.skip_varint()?;
            }
            let container = match code {
                1 | 2 => continue,
                3 => {
                    r.take_array::<1>()?;
                    continue;
                }
                4..=6 => {
                    r.skip_varint()?;
                    continue;
                }
                7 => {
                    r.take_array::<8>()?;
                    continue;
                }
                8 => {
                    r.skip_binary()?;
                    continue;
                }
                13 => {
                    r.take_array::<16>()?;
                    continue;
                }
                9 | 10 => ElementType::List,
                11 => ElementType::Map,
                12 => ElementType::Struct,
                _ => return Err(ThriftError::InvalidType { offset, code }),
            };
            self.pos = r.pos;
            self.skip_nested(container, depth + 1)?;
            r.pos = self.pos;
        }
    }

    /// Skips a map: a size varint, then (if non-empty) one byte of key and value
    /// types, then the entries.
    fn skip_map(&mut self, depth: u16) -> Result<(), ThriftError> {
        let offset = self.offset();
        let size = self.read_size(offset)?;
        if size == 0 {
            return Ok(());
        }
        let types_offset = self.offset();
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
                offset: self.offset(),
                limit: MAX_SKIP_DEPTH,
            });
        }
        Ok(())
    }

    /// Reads a non-zigzag varint holding a container size or binary length.
    fn read_size(&mut self, offset: u32) -> Result<u32, ThriftError> {
        let raw = self.read_varint::<32>(VarintTarget::U32)?;
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
    fn check_fits(&self, size: u32, min_len: u64, offset: u32) -> Result<(), ThriftError> {
        let remaining = self.input.len() - self.pos;
        // `min_len` is at most 32, so this cannot overflow.
        let needed = u64::from(size) * min_len;
        if needed > u64::try_from(remaining).unwrap_or(u64::MAX) {
            return Err(ThriftError::SizeExceedsInput {
                offset,
                size,
                remaining: saturate_u32(remaining),
            });
        }
        Ok(())
    }

    /// Reads an unsigned LEB128 varint whose value must fit in `BITS` bits.
    ///
    /// Padded encodings (extra `0x80` continuation bytes) are accepted, as the
    /// reference implementations do, as long as they stay within the byte limit for
    /// the width. Bits beyond the width are rejected rather than discarded.
    fn read_varint<const BITS: u32>(&mut self, target: VarintTarget) -> Result<u64, ThriftError> {
        let offset = self.offset();
        let first = self.read_byte()?;
        // Fast path: a value below 0x80 is one byte and fits every width.
        if first < 0x80 {
            return Ok(u64::from(first));
        }
        let mut value = u64::from(first & 0x7F);
        let mut shift = 7_u32;
        loop {
            if shift >= BITS {
                return Err(ThriftError::VarintOverflow { offset, target });
            }
            let byte = self.read_byte()?;
            let payload = u64::from(byte & 0x7F);
            let room = BITS - shift;
            if room < 7 && payload >> room != 0 {
                return Err(ThriftError::VarintOverflow { offset, target });
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// Skips a varint without decoding or width-checking it: everything up to and
    /// including the first byte without the continuation bit.
    fn skip_varint(&mut self) -> Result<(), ThriftError> {
        let rest = &self.input[self.pos..];
        // Fast path: most varints in footers are one byte.
        if rest.first().is_some_and(|&byte| byte < 0x80) {
            self.pos += 1;
            return Ok(());
        }
        let last = rest
            .iter()
            .position(|&byte| byte & 0x80 == 0)
            .ok_or_else(|| eof(self.input.len(), 1))?;
        self.pos += last + 1;
        Ok(())
    }

    /// Skips a binary value: its length varint and that many bytes. Like
    /// [`Self::read_binary`] but without building the slice.
    // Benchmarked: faster than `#[inline]` in skip.
    #[inline(always)]
    fn skip_binary(&mut self) -> Result<(), ThriftError> {
        let offset = self.offset();
        let size = self.read_size(offset)?;
        let remaining = self.input.len() - self.pos;
        match usize::try_from(size) {
            Ok(len) if len <= remaining => {
                self.pos += len;
                Ok(())
            }
            _ => Err(size_exceeds_input(offset, size, remaining)),
        }
    }

    fn read_byte(&mut self) -> Result<u8, ThriftError> {
        let byte = *self.input.get(self.pos).ok_or_else(|| eof(self.pos, 1))?;
        self.pos += 1;
        Ok(byte)
    }

    fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N], ThriftError> {
        let bytes = self.input[self.pos..]
            .first_chunk::<N>()
            .ok_or_else(|| eof(self.pos, N))?;
        self.pos += N;
        Ok(bytes)
    }

    /// The current position as an error offset.
    fn offset(&self) -> u32 {
        saturate_u32(self.pos)
    }
}

/// Builds an end-of-input error. Cold, to keep it out of hot loops (benchmarked).
#[cold]
fn eof(offset: usize, needed: usize) -> ThriftError {
    ThriftError::UnexpectedEof {
        offset: saturate_u32(offset),
        needed: saturate_u32(needed),
    }
}

/// Builds a size-exceeds-input error. Cold; see [`eof`].
#[cold]
fn size_exceeds_input(offset: u32, size: u32, remaining: usize) -> ThriftError {
    ThriftError::SizeExceedsInput {
        offset,
        size,
        remaining: saturate_u32(remaining),
    }
}

/// Converts an offset or length to `u32` for an error, saturating at `u32::MAX`.
///
/// Errors store `u32` offsets to stay 16 bytes. Ceiling: an offset is exact only below
/// 4 GiB into the reader's input. A footer can never reach that (its length field is a
/// `u32`), and page headers are small; only a caller handing one reader more than
/// 4 GiB could see a saturated offset. Offsets are diagnostics, never control flow.
fn saturate_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
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
