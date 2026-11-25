//! Unsigned LEB128 varint, zigzag-encoded integer, and container-size decoding for the
//! Thrift compact protocol.

use std::num::NonZeroUsize;

use super::errors::{ThriftError, VarintTarget};
use super::reader::{CompactReader, SIZE_MAX, eof, saturate_u32};

// `read_size_fast_cold` returns `(size, next_pos)` in two registers without an sret slot.
const _: () = assert!(size_of::<Option<(u32, NonZeroUsize)>>() <= size_of::<usize>() * 2);

/// Unsigned integer types that carry a Thrift LEB128 varint and its zigzag-encoded signed
/// counterpart.
pub(super) trait UnsignedVarint: Copy {
    type Signed;
    const BITS: u32;

    fn from_u8(byte: u8) -> Self;
    fn shl_or(self, payload: u8, shift: u32) -> Self;

    /// Reverses Thrift's zigzag encoding: `(n >> 1) ^ -(n & 1)`.
    fn zigzag_decode(self) -> Self::Signed;
}

macro_rules! impl_unsigned_varint {
    ($($unsigned:ty => $signed:ty),+ $(,)?) => {
        $(
            impl UnsignedVarint for $unsigned {
                type Signed = $signed;
                const BITS: u32 = <$unsigned>::BITS;

                #[inline(always)]
                fn from_u8(byte: u8) -> Self {
                    Self::from(byte)
                }

                #[inline(always)]
                fn shl_or(self, payload: u8, shift: u32) -> Self {
                    self | (Self::from(payload) << shift)
                }

                #[inline(always)]
                fn zigzag_decode(self) -> $signed {
                    ((self >> 1) ^ (self & 1).wrapping_neg()).cast_signed()
                }
            }
        )+
    };
}

impl_unsigned_varint!(u16 => i16, u32 => i32, u64 => i64);

/// Builds a varint-overflow error. Cold, to keep it out of hot loops.
#[cold]
fn varint_overflow(start: usize, target: VarintTarget) -> ThriftError {
    ThriftError::VarintOverflow {
        offset: saturate_u32(start),
        target,
    }
}

/// Builds a size-overflow error. Cold, to keep it out of hot loops.
#[cold]
fn size_overflow(header_start: usize, size: u32) -> ThriftError {
    ThriftError::SizeOverflow {
        offset: saturate_u32(header_start),
        size,
    }
}

impl CompactReader<'_> {
    /// Reads a non-zigzag varint holding a container size or binary length.
    /// `header_start` is the offset of the header carrying this size (used for
    /// `SizeOverflow`).
    pub(super) fn read_size(&mut self, header_start: usize) -> Result<u32, ThriftError> {
        let size = self.read_varint::<u32>(VarintTarget::U32)?;
        if size > SIZE_MAX {
            return Err(size_overflow(header_start, size));
        }
        Ok(size)
    }

    /// Reads an unsigned LEB128 varint whose value must fit in `T::BITS` bits.
    ///
    /// Padded encodings (extra `0x80` continuation bytes) are accepted, as the
    /// reference implementations do, as long as they stay within the byte limit for
    /// the width. Bits beyond the width are rejected rather than discarded.
    pub(super) fn read_varint<T: UnsignedVarint>(
        &mut self,
        target: VarintTarget,
    ) -> Result<T, ThriftError> {
        let start = self.position;
        let Some(&first) = self.input.get(start) else {
            return Err(eof(start, 1));
        };
        // Fast path: a value below 0x80 is one byte and fits every width.
        if first < 0x80 {
            self.position = start + 1;
            return Ok(T::from_u8(first));
        }
        let mut value = T::from_u8(first & 0x7F);
        let mut pos = start + 1;
        let mut shift = 7_u32;
        loop {
            if shift >= T::BITS {
                return Err(varint_overflow(start, target));
            }
            let Some(&byte) = self.input.get(pos) else {
                return Err(eof(pos, 1));
            };
            pos += 1;
            let payload = byte & 0x7F;
            let room = T::BITS - shift;
            if room < 7 && payload >> room != 0 {
                return Err(varint_overflow(start, target));
            }
            value = value.shl_or(payload, shift);
            if byte & 0x80 == 0 {
                self.position = pos;
                return Ok(value);
            }
            shift += 7;
        }
    }
}

/// Cold `u32` size varint reader returning `(size, next_pos)` in registers, or `None`.
#[cold]
#[inline(never)]
pub(super) fn read_size_fast_cold(input: &[u8], pos: usize) -> Option<(u32, NonZeroUsize)> {
    let mut reader = CompactReader::new(input, pos);
    let size = reader.read_size(0).ok()?;
    Some((size, NonZeroUsize::new(reader.position)?))
}

/// Decodes a multi-byte zigzag `i64` varint from `input[start..]`, returning
/// `(value, byte_len)` in registers, or `(0, 0)` on EOF or overflow.
///
/// Out of line so multi-field decoders stay compact, and takes `input` + `start` by value
/// so callers keep their reader in registers without a 16-byte stack return slot.
/// Callers handle single-byte varints (`byte < 0x80`) inline before calling here.
#[inline(never)]
pub(super) fn decode_i64(input: &[u8], start: usize) -> (i64, u8) {
    let Some(rest) = input.get(start..) else {
        return (0, 0);
    };
    if let Some(&bytes) = rest.first_chunk::<8>() {
        let word = u64::from_le_bytes(bytes);
        debug_assert!(word & 0x80 != 0);
        let mut raw = word & 0x7F;
        for byte_index in 1..8_u8 {
            let bit = u32::from(byte_index) * 8;
            raw |= ((word >> bit) & 0x7F) << (u32::from(byte_index) * 7);
            if word & (0x80_u64 << bit) == 0 {
                return (raw.zigzag_decode(), byte_index + 1);
            }
        }
    }
    decode_i64_tail(rest)
}

/// Fallback for [`decode_i64`] when fewer than 8 bytes remain or a varint spans 9 or 10
/// bytes. Reuses [`CompactReader::read_varint`] so overflow and padding rules stay in one
/// place.
#[cold]
#[inline(never)]
fn decode_i64_tail(rest: &[u8]) -> (i64, u8) {
    let mut reader = CompactReader::new(rest, 0);
    if let Ok(raw) = reader.read_varint::<u64>(VarintTarget::I64) {
        if let Ok(len) = u8::try_from(reader.position) {
            return (raw.zigzag_decode(), len);
        }
        debug_assert!(false, "a valid 64-bit varint is at most 10 bytes");
    }
    (0, 0)
}

/// Diagnoses why [`decode_i64`] failed at `start`. Kept cold so the hot varint path
/// never builds a 16-byte `Result`.
#[cold]
#[inline(never)]
pub(super) fn varint_i64_error(input: &[u8], start: usize) -> ThriftError {
    if let Err(err) = CompactReader::new(input, start).read_varint::<u64>(VarintTarget::I64) {
        return err;
    }
    debug_assert!(false, "varint_i64_error called on valid varint");
    varint_overflow(start, VarintTarget::I64)
}
