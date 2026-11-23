//! Decoding of the Thrift compact protocol, which Parquet uses for its footer and
//! page headers.
//!
//! This layer knows nothing about Parquet's own structs; those decoders are built on
//! top of it. It reads from a borrowed buffer and returns an error on any malformed
//! input. It allocates nothing, except that [`CompactReader::skip_marking`] grows the
//! [`Marks`] its caller owns.
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
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

pub use errors::{ThriftError, VarintTarget};

/// Maximum container nesting that [`CompactReader::skip`] descends into.
///
/// Parquet's own structs nest only a few levels, so this bounds the recursion an
/// attacker can cause through unknown fields without rejecting real files.
pub const SKIP_DEPTH_MAX: u16 = 64;

/// Largest container size or binary length the protocol allows (the maximum `i32`).
const SIZE_MAX: u32 = i32::MAX.cast_unsigned();

/// The largest minimum length `check_fits` is ever given: one map entry whose key and
/// value are both UUIDs.
const ELEMENT_LEN_MIN_MAX: u64 = 32;

/// The fewest zeros a mask is extended by when marking reaches its end. Marks are
/// written a word at a time, so it must be at least a word.
const MASK_GROWTH_SIZE_MIN: usize = 4096;

// `skip` must be able to descend at least one level, or every container is rejected.
const _: () = assert!(SKIP_DEPTH_MAX > 0);
const _: () = assert!(MASK_GROWTH_SIZE_MIN >= size_of::<u64>());
// `check_fits` multiplies a size by an element length; the product must fit in a `u64`.
const _: () = assert!(ElementType::Uuid.encoded_len_min() * 2 == ELEMENT_LEN_MIN_MAX);
const _: () = assert!((SIZE_MAX as u64).checked_mul(ELEMENT_LEN_MIN_MAX).is_some());

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
    const fn encoded_len_min(self) -> u64 {
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
    field_type: FieldType,
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
    /// A cursor that continues a struct after the field with id `last_id`.
    ///
    /// Field ids are written as deltas from the previous one, so resuming part of the
    /// way through a struct needs the id that came last. [`FieldCursor::default`]
    /// starts a struct from its first field.
    #[must_use]
    pub const fn after(last_id: i16) -> Self {
        Self { last_id }
    }

    /// Reads the next field header, or returns `None` at the struct's stop marker.
    ///
    /// After `None`, the struct is finished and the cursor must not be used again.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, the type code is invalid, or the field id
    /// overflows `i16`.
    // Inlined into the decoders, with `read_i32`, `read_i64`, `read_binary`, and
    // `skip_field`: as calls they made extraction a quarter to a third slower
    // (`docs/perf/open.md`).
    #[inline]
    pub fn next(&mut self, reader: &mut CompactReader<'_>) -> Result<Option<Field>, ThriftError> {
        let offset = reader.offset();
        let byte = reader.read_byte()?;
        // Like the reference implementations, only the low nibble decides the stop code.
        let code = byte & 0x0F;
        if code == 0 {
            return Ok(None);
        }
        let field_type = FieldType::from_code(code, offset)?;
        let delta = byte >> 4;
        let id = if delta == 0 {
            reader.read_i16()?
        } else {
            self.last_id
                .checked_add(i16::from(delta))
                .ok_or(ThriftError::FieldIdOverflow { offset })?
        };
        self.last_id = id;
        Ok(Some(Field { id, field_type }))
    }
}

/// Reads Thrift compact-protocol values from a borrowed buffer.
#[derive(Debug, Clone)]
pub struct CompactReader<'a> {
    input: &'a [u8],
    /// Offset of the next unread byte. Invariant: `position <= input.len()`.
    position: usize,
}

impl<'a> CompactReader<'a> {
    /// Creates a reader over `input` whose next unread byte is at `position`.
    ///
    /// Every error offset is relative to the start of `input`, wherever reading begins,
    /// so a caller that decodes one structure out of a larger buffer passes the whole
    /// buffer and the structure's position, not a slice.
    ///
    /// `position` must not exceed the length of `input`.
    #[must_use]
    pub const fn new(input: &'a [u8], position: usize) -> Self {
        debug_assert!(position <= input.len());
        Self { input, position }
    }

    /// Offset of the next unread byte within the input.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    /// The number of bytes not yet read.
    #[must_use]
    pub const fn remaining_size(&self) -> usize {
        self.input.len() - self.position
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
    #[inline]
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
    #[inline]
    pub fn read_i64(&mut self) -> Result<i64, ThriftError> {
        let raw = self.read_varint::<64>(VarintTarget::I64)?;
        Ok((raw >> 1).cast_signed() ^ (raw & 1).cast_signed().wrapping_neg())
    }

    /// Reads a double, stored as 8 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the input has fewer than 8 bytes left.
    #[cfg_attr(
        not(any(test, feature = "internals")),
        expect(
            dead_code,
            reason = "the only doubles in the format are geospatial bounds, which are not decoded yet"
        )
    )]
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
    #[inline]
    pub fn read_binary(&mut self) -> Result<&'a [u8], ThriftError> {
        let offset = self.offset();
        let size = self.read_size(offset)?;
        let rest = &self.input[self.position..];
        let bytes = usize::try_from(size)
            .ok()
            .and_then(|len| rest.get(..len))
            .ok_or_else(|| ThriftError::SizeExceedsInput {
                offset,
                size,
                remaining: saturate_u32(rest.len()),
            })?;
        self.position += bytes.len();
        debug_assert!(self.position <= self.input.len());
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
    #[cfg_attr(
        not(any(test, feature = "internals")),
        expect(
            dead_code,
            reason = "the only bool lists in the format are in the page index, which is not decoded yet"
        )
    )]
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
        self.check_fits(size, element_type.encoded_len_min(), offset)?;
        // Every size this reader returns fits in the input that is left.
        debug_assert!(
            u64::from(size) * element_type.encoded_len_min()
                <= u64::try_from(self.input.len() - self.position).unwrap_or(u64::MAX)
        );
        Ok(ListHeader { element_type, size })
    }

    /// Skips the value of a struct field. A bool field's value is in its header, so
    /// skipping it consumes nothing.
    ///
    /// # Errors
    ///
    /// Returns any error [`CompactReader::skip`] would.
    #[inline]
    pub fn skip_field(&mut self, field_type: FieldType) -> Result<(), ThriftError> {
        match field_type {
            // An integer is passed over here, without a call into the general skip:
            // most fields a decoder does not want are integers.
            FieldType::I16 | FieldType::I32 | FieldType::I64 => self.skip_varint(&mut Unmarked),
            FieldType::Bool(_)
            | FieldType::I8
            | FieldType::Double
            | FieldType::Binary
            | FieldType::List
            | FieldType::Set
            | FieldType::Map
            | FieldType::Struct
            | FieldType::Uuid => match field_type.element_type() {
                Some(element_type) => self.skip(element_type),
                None => Ok(()),
            },
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
    /// Containers are descended into at most [`SKIP_DEPTH_MAX`] levels deep.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, a type code or size is invalid, or
    /// containers nest too deeply.
    pub fn skip(&mut self, element_type: ElementType) -> Result<(), ThriftError> {
        self.skip_nested(element_type, 0, &mut Unmarked)
    }

    /// Skips one value as [`skip`](Self::skip) does, and appends to `marks` which bits
    /// of the value decided where it ends. See [`Marks`].
    ///
    /// This accepts exactly what `skip` accepts and ends where `skip` ends: it is the
    /// same code. It is slower, so it is for the few values that others are compared
    /// with.
    ///
    /// # Errors
    ///
    /// Returns any error [`skip`](Self::skip) would, and then leaves `marks` as it was.
    pub fn skip_marking(
        &mut self,
        element_type: ElementType,
        marks: &mut Marks,
    ) -> Result<(), ThriftError> {
        let input_start = self.position;
        let mask_start = marks.mask_size;
        let binaries_start = marks.binaries.len();
        let mut marking = Marking {
            marks: &mut *marks,
            // Wrapping, because the mask may be shorter than the input skipped so far.
            shift: mask_start.wrapping_sub(input_start),
        };
        let skipped = self.skip_nested(element_type, 0, &mut marking);
        if skipped.is_ok() {
            // One mark for each byte passed over. The value may end with contents,
            // which have no marks written, beyond the zeros the mask already has.
            let mask_end = mask_start + (self.position - input_start);
            if marks.mask.len() < mask_end {
                marks.mask.resize(mask_end, Marks::CONTENT);
            } else {
                // The mask already reaches that far.
            }
            marks.mask_size = mask_end;
        } else {
            // Everything past the marks that were there is zeros again.
            marks.mask[mask_start..].fill(Marks::CONTENT);
            marks.binaries.truncate(binaries_start);
        }
        skipped
    }

    /// Skips one value `depth` containers down, telling `marker` of the bytes it reads.
    fn skip_nested<M: Marker>(
        &mut self,
        element_type: ElementType,
        depth: u16,
        marker: &mut M,
    ) -> Result<(), ThriftError> {
        // `check_depth` stops a container at the limit, so nothing is called deeper.
        debug_assert!(depth <= SKIP_DEPTH_MAX);
        match element_type {
            ElementType::List | ElementType::Set => {
                self.check_depth(depth)?;
                let header_start = self.position;
                let header = self.read_list_header()?;
                marker.structure(header_start, self.position);
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
                            self.skip_varint(marker)?;
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
                            self.skip_binary(marker)?;
                        }
                    }
                    container => {
                        for _ in 0..size {
                            self.skip_nested(container, depth + 1, marker)?;
                        }
                    }
                }
                Ok(())
            }
            ElementType::Map => {
                self.check_depth(depth)?;
                self.skip_map(depth, marker)
            }
            ElementType::Struct => {
                self.check_depth(depth)?;
                self.skip_struct_fields(depth, marker)
            }
            ElementType::Bool | ElementType::I8 => self.take_array::<1>().map(drop),
            ElementType::I16 | ElementType::I32 | ElementType::I64 => self.skip_varint(marker),
            ElementType::Double => self.take_array::<8>().map(drop),
            ElementType::Uuid => self.take_array::<16>().map(drop),
            ElementType::Binary => self.skip_binary(marker),
        }
    }

    /// Skips struct fields up to and including the stop marker. Like
    /// [`FieldCursor::next`], but field ids are not needed, so a long-form id is
    /// skipped instead of decoded. Scalar fields are skipped inline, dispatching once
    /// on the raw type code; only containers recurse.
    fn skip_struct_fields<M: Marker>(
        &mut self,
        depth: u16,
        marker: &mut M,
    ) -> Result<(), ThriftError> {
        // The caller passed `check_depth`, so one more level is within the limit.
        debug_assert!(depth < SKIP_DEPTH_MAX);
        // A local copy keeps the position in a register (benchmarked); `self` is synced
        // only around recursive calls.
        let mut reader = self.clone();
        loop {
            let field_position = reader.position;
            let offset = reader.offset();
            let byte = reader.read_byte()?;
            // Every field consumes at least its header byte, so the loop is bounded by
            // the length of the input.
            debug_assert!(reader.position > field_position);
            marker.structure(field_position, reader.position);
            let code = byte & 0x0F;
            if code == 0 {
                self.position = reader.position;
                return Ok(());
            }
            if byte >> 4 == 0 {
                reader.skip_varint(marker)?;
            }
            let container = match code {
                1 | 2 => continue,
                3 => {
                    reader.take_array::<1>()?;
                    continue;
                }
                4..=6 => {
                    reader.skip_varint(marker)?;
                    continue;
                }
                7 => {
                    reader.take_array::<8>()?;
                    continue;
                }
                8 => {
                    reader.skip_binary(marker)?;
                    continue;
                }
                13 => {
                    reader.take_array::<16>()?;
                    continue;
                }
                9 | 10 => ElementType::List,
                11 => ElementType::Map,
                12 => ElementType::Struct,
                _ => return Err(ThriftError::InvalidType { offset, code }),
            };
            self.position = reader.position;
            self.skip_nested(container, depth + 1, marker)?;
            debug_assert!(self.position >= reader.position);
            reader.position = self.position;
        }
    }

    /// Skips a map: a size varint, then (if non-empty) one byte of key and value
    /// types, then the entries.
    fn skip_map<M: Marker>(&mut self, depth: u16, marker: &mut M) -> Result<(), ThriftError> {
        // The caller passed `check_depth`, so one more level is within the limit.
        debug_assert!(depth < SKIP_DEPTH_MAX);
        let header_start = self.position;
        let offset = self.offset();
        let size = self.read_size(offset)?;
        if size == 0 {
            marker.structure(header_start, self.position);
            return Ok(());
        }
        let types_offset = self.offset();
        let types = self.read_byte()?;
        let key = ElementType::from_code(types >> 4, types_offset)?;
        let value = ElementType::from_code(types & 0x0F, types_offset)?;
        self.check_fits(
            size,
            key.encoded_len_min() + value.encoded_len_min(),
            offset,
        )?;
        marker.structure(header_start, self.position);
        for _ in 0..size {
            self.skip_nested(key, depth + 1, marker)?;
            self.skip_nested(value, depth + 1, marker)?;
        }
        Ok(())
    }

    fn check_depth(&self, depth: u16) -> Result<(), ThriftError> {
        if depth >= SKIP_DEPTH_MAX {
            return Err(ThriftError::DepthExceeded {
                offset: self.offset(),
                limit: SKIP_DEPTH_MAX,
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
        if size > SIZE_MAX {
            return Err(ThriftError::SizeOverflow { offset, size });
        }
        Ok(size)
    }

    /// Checks that `size` values of at least `element_len_min` bytes each fit in the
    /// rest of the input. `offset` is where the header holding `size` starts.
    fn check_fits(&self, size: u32, element_len_min: u64, offset: u32) -> Result<(), ThriftError> {
        // Within these bounds the product below cannot overflow; see the constants.
        debug_assert!(element_len_min >= 1);
        debug_assert!(element_len_min <= ELEMENT_LEN_MIN_MAX);
        debug_assert!(size <= SIZE_MAX);
        let remaining = self.input.len() - self.position;
        let needed = u64::from(size) * element_len_min;
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
        // The shift arithmetic below assumes one of the three widths the protocol uses.
        const { assert!(BITS == 16 || BITS == 32 || BITS == 64) };
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
                // Callers truncate the result to `BITS` bits and rely on this.
                debug_assert!(BITS == 64 || value >> BITS == 0);
                return Ok(value);
            }
            shift += 7;
        }
    }

    /// Skips a varint without decoding or width-checking it: everything up to and
    /// including the first byte without the continuation bit.
    // Always inlined, so that marking has no call for each integer. The plain skip
    // inlined it already: its machine code is the same with and without this.
    #[inline(always)]
    fn skip_varint<M: Marker>(&mut self, marker: &mut M) -> Result<(), ThriftError> {
        let rest = &self.input[self.position..];
        // Fast path: most varints in footers are one byte.
        if rest.first().is_some_and(|&byte| byte < 0x80) {
            marker.varint(self.position, self.position + 1);
            self.position += 1;
            return Ok(());
        }
        let last = rest
            .iter()
            .position(|&byte| byte & 0x80 == 0)
            .ok_or_else(|| eof(self.input.len(), 1))?;
        marker.varint(self.position, self.position + last + 1);
        self.position += last + 1;
        debug_assert!(self.position <= self.input.len());
        Ok(())
    }

    /// Skips a binary value: its length varint and that many bytes. Like
    /// [`Self::read_binary`] but without building the slice.
    // Benchmarked: faster than `#[inline]` in skip.
    #[inline(always)]
    fn skip_binary<M: Marker>(&mut self, marker: &mut M) -> Result<(), ThriftError> {
        let length_start = self.position;
        let offset = self.offset();
        let size = self.read_size(offset)?;
        marker.binary(length_start, self.position);
        let remaining = self.input.len() - self.position;
        match usize::try_from(size) {
            Ok(len) if len <= remaining => {
                self.position += len;
                Ok(())
            }
            _ => Err(size_exceeds_input(offset, size, remaining)),
        }
    }

    fn read_byte(&mut self) -> Result<u8, ThriftError> {
        let byte = *self
            .input
            .get(self.position)
            .ok_or_else(|| eof(self.position, 1))?;
        self.position += 1;
        Ok(byte)
    }

    fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N], ThriftError> {
        let bytes = self.input[self.position..]
            .first_chunk::<N>()
            .ok_or_else(|| eof(self.position, N))?;
        self.position += N;
        Ok(bytes)
    }

    /// The current position as an error offset.
    fn offset(&self) -> u32 {
        saturate_u32(self.position)
    }
}

/// Which bits of skipped values decided where the values end, as
/// [`CompactReader::skip_marking`] records them.
///
/// Skipping reads some bits of its input and not others. It reads every bit of a field
/// header or of a size, only the continuation bit of each byte of an integer, and
/// nothing of a binary's contents. So bytes that agree with a skipped value on every
/// bit that was read are skipped by the same steps, and hold a value of the same
/// length. A caller can pass over them by comparing instead of parsing.
///
/// Two things can differ further without changing the steps: an integer can have any
/// length, and so can a binary. The mask shows where the integers are and the binaries
/// are listed, so a caller that finds a difference in one of them can step over it in
/// both values and go on comparing.
#[derive(Debug, Default)]
pub struct Marks {
    /// The mask, and after it zeros for marks to come to be written into.
    mask: Vec<u8>,
    /// How much of `mask` is marks.
    mask_size: usize,
    binaries: Vec<u32>,
}

impl Marks {
    /// In the mask: every bit of the byte was read. Field headers, stop markers, list
    /// and map headers, and the sizes of lists and binaries are marked this way.
    pub const STRUCTURE: u8 = 0xFF;
    /// In the mask: only the continuation bit was read. This is a byte of an integer's
    /// varint, whose other seven bits are part of the integer.
    pub const VARINT: u8 = 0x80;
    /// In the mask: no bit was read. This is a byte of a binary's contents, or of a
    /// value of fixed width.
    pub const CONTENT: u8 = 0x00;

    /// Marks of nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mask: Vec::new(),
            mask_size: 0,
            binaries: Vec::new(),
        }
    }

    /// Forgets every mark, and keeps the memory for the next ones.
    #[cfg_attr(
        not(feature = "internals"),
        expect(
            dead_code,
            reason = "used by the benchmarks, which mark one footer many times"
        )
    )]
    pub fn clear(&mut self) {
        self.mask.clear();
        self.mask_size = 0;
        self.binaries.clear();
    }

    /// One byte for each byte skipped, in order, with the bits set that were read.
    #[must_use]
    pub const fn mask(&self) -> &[u8] {
        self.mask.as_slice().split_at(self.mask_size).0
    }

    /// For each binary skipped, in order, the index in [`mask`](Self::mask) of the
    /// first byte of its length.
    ///
    /// A binary past the first 4 GiB of mask is not listed. That only keeps a caller
    /// from stepping over it.
    #[must_use]
    pub const fn binaries(&self) -> &[u32] {
        self.binaries.as_slice()
    }
}

/// What a skip tells of the bytes it reads. It tells of them in the order it reads
/// them. A plain skip tells nothing, and compiles to the same code as if this were not
/// here.
trait Marker {
    /// Every bit of `input[start..end]` was read.
    fn structure(&mut self, start: usize, end: usize);
    /// Only the continuation bits of `input[start..end]`, one integer's varint, were.
    fn varint(&mut self, start: usize, end: usize);
    /// `input[start..end]` is the length of a binary, and every bit of it was read.
    fn binary(&mut self, start: usize, end: usize);
}

/// The marker of a plain skip.
struct Unmarked;

impl Marker for Unmarked {
    fn structure(&mut self, _start: usize, _end: usize) {}
    fn varint(&mut self, _start: usize, _end: usize) {}
    fn binary(&mut self, _start: usize, _end: usize) {}
}

/// The marker of [`CompactReader::skip_marking`].
struct Marking<'a> {
    marks: &'a mut Marks,
    /// What to add to an input position, wrapping, for the index of its mark.
    shift: usize,
}

impl Marking<'_> {
    /// A word with [`Marks::STRUCTURE`] in every byte.
    const STRUCTURE: u64 = u64::from_le_bytes([Marks::STRUCTURE; 8]);
    /// A word with [`Marks::VARINT`] in every byte.
    const VARINT: u64 = u64::from_le_bytes([Marks::VARINT; 8]);

    /// For each count of bytes up to a word, a word with all bits set in that many of
    /// its first bytes.
    const KEPT: [u64; 9] = [
        0,
        0xFF,
        0xFFFF,
        0xFF_FFFF,
        0xFFFF_FFFF,
        0xFF_FFFF_FFFF,
        0xFFFF_FFFF_FFFF,
        0xFF_FFFF_FFFF_FFFF,
        u64::MAX,
    ];

    /// Marks `input[start..end]` with the byte that `marks` repeats.
    // Always inlined, with the rare cases kept out of line. Left to the compiler, each
    // mark is a call, and marking costs well over twice what skipping does
    // (`skip_marking` in `docs/perf/open.md`).
    #[inline(always)]
    fn set(&mut self, start: usize, end: usize, marks: u64) {
        debug_assert!(start < end);
        let index = start.wrapping_add(self.shift);
        let count = end - start;
        let word = self
            .marks
            .mask
            .get_mut(index..)
            .and_then(|rest| rest.first_chunk_mut::<8>());
        match (word, Self::KEPT.get(count)) {
            (Some(word), Some(kept)) => {
                // Marks are set in the order of the input, so everything from here on
                // is still zero, and a whole word can be written: the bytes of it past
                // these marks stay zero.
                debug_assert_eq!(*word, [Marks::CONTENT; 8]);
                *word = (marks & kept).to_le_bytes();
            }
            _ => self.set_slowly(index, count, marks),
        }
    }

    /// [`Self::set`] when the mask has no room for a word, or the marks are more than
    /// a word: a varint padded beyond any integer's length, which `skip` accepts.
    #[cold]
    #[inline(never)]
    fn set_slowly(&mut self, index: usize, count: usize, marks: u64) {
        let mask = &mut self.marks.mask;
        let needed = index + count.max(size_of::<u64>());
        if mask.len() < needed {
            // Grow by half, so that extending the mask costs a fixed amount for each
            // byte of it however many values are marked.
            let grown = (needed + MASK_GROWTH_SIZE_MIN).max(mask.len() + mask.len() / 2);
            mask.resize(grown, Marks::CONTENT);
        } else {
            // There is room: the marks are just longer than a word.
        }
        mask[index..index + count].fill(marks.to_le_bytes()[0]);
    }
}

impl Marker for Marking<'_> {
    #[inline(always)]
    fn structure(&mut self, start: usize, end: usize) {
        self.set(start, end, Self::STRUCTURE);
    }

    #[inline(always)]
    fn varint(&mut self, start: usize, end: usize) {
        self.set(start, end, Self::VARINT);
    }

    #[inline(always)]
    fn binary(&mut self, start: usize, end: usize) {
        if let Ok(index) = u32::try_from(start.wrapping_add(self.shift)) {
            self.marks.binaries.push(index);
        } else {
            // Not listed: see `Marks::binaries`.
        }
        self.set(start, end, Self::STRUCTURE);
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
fn saturate_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
