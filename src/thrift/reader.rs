//! [`CompactReader`] and [`FieldCursor`]: value-reading methods for the Thrift compact
//! protocol.

use super::errors::{ThriftError, VarintTarget};
use super::types::{ElementType, Field, FieldType, ListHeader, wire_type};
use super::varint::{UnsignedVarint, decode_i64, varint_i64_error};

/// Largest container size or binary length the protocol allows (the maximum `i32`).
pub(super) const SIZE_MAX: u32 = i32::MAX.cast_unsigned();

/// The largest minimum length `check_fits` is ever given: one map entry whose key and
/// value are both UUIDs.
const ELEMENT_LEN_MIN_MAX: u64 = 32;

// `check_fits` multiplies a size by an element length; the product must fit in a `u64`.
const _: () = assert!(ElementType::Uuid.encoded_len_min() * 2 == ELEMENT_LEN_MIN_MAX);
const _: () = assert!((SIZE_MAX as u64).checked_mul(ELEMENT_LEN_MIN_MAX).is_some());

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
        let start = reader.position;
        let byte = reader.read_byte()?;
        // Like the reference implementations, only the low nibble decides the stop code.
        let code = byte & 0x0F;
        if code == 0 {
            return Ok(None);
        }
        let field_type = FieldType::from_code(code, start)?;
        let delta = byte >> 4;
        let id = if delta == 0 {
            reader.read_i16()?
        } else {
            self.last_id
                .checked_add(i16::from(delta))
                .ok_or_else(|| field_id_overflow(start))?
        };
        self.last_id = id;
        Ok(Some(Field { id, field_type }))
    }
}

/// Reads Thrift compact-protocol values from a borrowed buffer.
#[derive(Debug, Clone)]
pub struct CompactReader<'a> {
    pub(super) input: &'a [u8],
    /// Offset of the next unread byte. Invariant: `position <= input.len()`.
    pub(super) position: usize,
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
        Ok(self.read_varint::<u16>(VarintTarget::I16)?.zigzag_decode())
    }

    /// Reads an `i32`, stored as a zigzag varint.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends or the varint overflows `i32`.
    #[inline]
    pub fn read_i32(&mut self) -> Result<i32, ThriftError> {
        Ok(self.read_varint::<u32>(VarintTarget::I32)?.zigzag_decode())
    }

    /// Reads an `i64`, stored as a zigzag varint.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends or the varint overflows `i64`.
    #[inline]
    pub fn read_i64(&mut self) -> Result<i64, ThriftError> {
        let start = self.position;
        if let Some(&first) = self.input.get(start)
            && first < 0x80
        {
            self.position = start + 1;
            return Ok(u64::from(first).zigzag_decode());
        }
        let (value, len) = decode_i64(self.input, start);
        if len == 0 {
            return Err(varint_i64_error(self.input, start));
        }
        self.position = start + usize::from(len);
        Ok(value)
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
        let start = self.position;
        if let Some(&first) = self.input.get(start)
            && first < 0x80
        {
            let data_start = start + 1;
            let end = data_start + usize::from(first);
            if let Some(bytes) = self.input.get(data_start..end) {
                self.position = end;
                return Ok(bytes);
            }
        }
        self.read_binary_slow()
    }

    /// Cold fallback for [`Self::read_binary`] handling multi-byte lengths (`>= 128` bytes),
    /// EOF, and truncated payloads.
    #[cold]
    #[inline(never)]
    fn read_binary_slow(&mut self) -> Result<&'a [u8], ThriftError> {
        let start = self.position;
        let size = self.read_size(start)?;
        let data_start = self.position;
        let Some(bytes) = usize::try_from(size)
            .ok()
            .and_then(|len| self.input.get(data_start..)?.get(..len))
        else {
            return Err(size_exceeds_input(
                start,
                size,
                self.input.len() - data_start,
            ));
        };
        self.position = data_start + bytes.len();
        debug_assert!(self.position <= self.input.len());
        Ok(bytes)
    }

    /// Fast path for reading a short (`< 15` elements) `List<I32>` whose elements are all
    /// single-byte varints (`< 0x80`). Folds each decoded `i32` with `fold` and advances
    /// past the list only if every element is 1 byte and `fold` returns `Some`; otherwise
    /// leaves `self` untouched and returns `None`.
    #[inline]
    pub(crate) fn try_fold_short_i32_list<T>(
        &mut self,
        init: T,
        mut fold: impl FnMut(T, i32) -> Option<T>,
    ) -> Option<T> {
        let rest = self.input.get(self.position..)?;
        let &header = rest.first()?;
        if header & 0x0F != wire_type::I32 || header >= 0xF0 {
            return None;
        }
        let len = usize::from(header >> 4);
        let elems = rest.get(1..1 + len)?;
        let mut acc = init;
        for &byte in elems {
            if byte >= 0x80 {
                return None;
            }
            acc = fold(acc, u32::from(byte).zigzag_decode())?;
        }
        self.position += 1 + len;
        Some(acc)
    }

    /// Fast path for reading a short (`< 15` elements) `List<Struct>` whose elements each
    /// hold exactly the `i32` fields 1, 2, and 3, in that order, the first two as
    /// single-byte varints (`< 0x80`) and the third in at most two bytes. Folds each
    /// element's three decoded values with `fold` and advances past the list only if
    /// every element has that shape and `fold` returns `Some`; otherwise leaves `self`
    /// untouched and returns `None`.
    #[inline]
    pub(crate) fn try_fold_short_i32_triple_list<T>(
        &mut self,
        init: T,
        mut fold: impl FnMut(T, [i32; 3]) -> Option<T>,
    ) -> Option<T> {
        /// The header of an `i32` field whose id is one above the field before it.
        const NEXT: u8 = 0x10 | wire_type::I32;
        let rest = self.input.get(self.position..)?;
        let (&header, mut elements) = rest.split_first()?;
        if header & 0x0F != wire_type::STRUCT || header >= 0xF0 {
            return None;
        }
        let mut acc = init;
        for _ in 0..header >> 4 {
            let (first, second, third, len) = match *elements {
                [NEXT, first, NEXT, second, NEXT, low, wire_type::STOP, ..] if low < 0x80 => {
                    (first, second, u32::from(low), 7)
                }
                [
                    NEXT,
                    first,
                    NEXT,
                    second,
                    NEXT,
                    low,
                    high,
                    wire_type::STOP,
                    ..,
                ] if low >= 0x80 && high < 0x80 => {
                    let third = u32::from(low & 0x7F) | (u32::from(high) << 7);
                    (first, second, third, 8)
                }
                _ => return None,
            };
            if first >= 0x80 || second >= 0x80 {
                return None;
            }
            let values = [u32::from(first), u32::from(second), third].map(u32::zigzag_decode);
            acc = fold(acc, values)?;
            elements = &elements[len..];
        }
        self.position += rest.len() - elements.len();
        Some(acc)
    }

    /// Fast path for skipping a short (`< 15` elements) `List<Binary>` whose elements
    /// each have a single-byte length (`< 0x80`). Advances past the list on success;
    /// leaves `self` untouched and returns `false` on any other case.
    #[inline]
    pub(crate) fn try_skip_short_binary_list(&mut self) -> bool {
        let Some(&header) = self.input.get(self.position) else {
            return false;
        };
        if header & 0x0F != wire_type::BINARY || header >= 0xF0 {
            return false;
        }
        let count = header >> 4;
        let mut pos = self.position + 1;
        for _ in 0..count {
            let Some(&len) = self.input.get(pos) else {
                return false;
            };
            if len >= 0x80 {
                return false;
            }
            pos += 1 + usize::from(len);
        }
        if pos <= self.input.len() {
            self.position = pos;
            true
        } else {
            false
        }
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
    // Benchmarked: kept inline for hot metadata decoders (`encodings_decode`, etc.).
    #[inline(always)]
    pub fn read_list_header(&mut self) -> Result<ListHeader, ThriftError> {
        let start = self.position;
        let byte = self.read_byte()?;
        if byte == 0 {
            return Ok(ListHeader {
                element_type: ElementType::I8,
                size: 0,
            });
        }
        let element_type = ElementType::from_code(byte & 0x0F, start)?;
        let short_size = byte >> 4;
        let size = if short_size == 0x0F {
            self.read_size(start)?
        } else {
            u32::from(short_size)
        };
        self.check_fits(size, element_type.encoded_len_min(), start)?;
        // Every size this reader returns fits in the input that is left.
        debug_assert!(
            u64::from(size) * element_type.encoded_len_min()
                <= u64::try_from(self.input.len() - self.position).unwrap_or(u64::MAX)
        );
        Ok(ListHeader { element_type, size })
    }

    /// Checks that `size` values of at least `element_len_min` bytes each fit in the
    /// rest of the input. `header_start` is where the header holding `size` starts.
    pub(super) fn check_fits(
        &self,
        size: u32,
        element_len_min: u64,
        header_start: usize,
    ) -> Result<(), ThriftError> {
        // Within these bounds the product below cannot overflow; see the constants.
        debug_assert!(element_len_min >= 1);
        debug_assert!(element_len_min <= ELEMENT_LEN_MIN_MAX);
        debug_assert!(size <= SIZE_MAX);
        let remaining = self.input.len() - self.position;
        let needed = u64::from(size) * element_len_min;
        if needed > u64::try_from(remaining).unwrap_or(u64::MAX) {
            return Err(size_exceeds_input(header_start, size, remaining));
        }
        Ok(())
    }

    pub(super) fn read_byte(&mut self) -> Result<u8, ThriftError> {
        let byte = *self
            .input
            .get(self.position)
            .ok_or_else(|| eof(self.position, 1))?;
        self.position += 1;
        Ok(byte)
    }

    pub(super) fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N], ThriftError> {
        let bytes = self
            .input
            .get(self.position..)
            .and_then(|rest| rest.first_chunk::<N>())
            .ok_or_else(|| eof(self.position, N))?;
        self.position += N;
        Ok(bytes)
    }

    /// The current position as an error offset.
    pub(super) fn offset(&self) -> u32 {
        saturate_u32(self.position)
    }
}

/// Builds an end-of-input error. Cold, to keep it out of hot loops (benchmarked).
#[cold]
pub(super) fn eof(offset: usize, needed: usize) -> ThriftError {
    ThriftError::UnexpectedEof {
        offset: saturate_u32(offset),
        needed: saturate_u32(needed),
    }
}

/// Builds an invalid-type error. Cold, to keep it out of hot loops (benchmarked).
#[cold]
pub(super) fn invalid_type(offset: usize, code: u8) -> ThriftError {
    ThriftError::InvalidType {
        offset: saturate_u32(offset),
        code,
    }
}

/// Builds a field-id-overflow error. Cold, to keep it out of hot loops (benchmarked).
#[cold]
fn field_id_overflow(offset: usize) -> ThriftError {
    ThriftError::FieldIdOverflow {
        offset: saturate_u32(offset),
    }
}

/// Builds a size-exceeds-input error. Cold; see [`eof`].
#[cold]
pub(super) fn size_exceeds_input(header_start: usize, size: u32, remaining: usize) -> ThriftError {
    ThriftError::SizeExceedsInput {
        offset: saturate_u32(header_start),
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
pub(super) fn saturate_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
