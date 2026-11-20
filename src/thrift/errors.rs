//! Errors from decoding the Thrift compact protocol.

use std::fmt;

use thiserror::Error;

/// The type a varint was being decoded into when it overflowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarintTarget {
    /// An `i16` value or field id.
    I16,
    /// An `i32` value.
    I32,
    /// An `i64` value.
    I64,
    /// A container size or binary length.
    U32,
}

impl fmt::Display for VarintTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U32 => "u32",
        })
    }
}

/// A malformed Thrift compact-protocol input.
///
/// Every variant names the byte offset, within the buffer given to
/// [`CompactReader::new`](super::CompactReader::new), at which the bad value starts.
/// Offsets are `u32` so the error stays 16 bytes; they saturate at `u32::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ThriftError {
    /// The input ended before a value was complete.
    #[error("unexpected end of input at byte {offset}: needed {needed} more bytes")]
    UnexpectedEof {
        /// Offset of the first missing byte.
        offset: u32,
        /// Number of bytes the value needed from that offset.
        needed: u32,
    },
    /// A varint ran past the width of its target type.
    #[error("varint at byte {offset} overflows {target}")]
    VarintOverflow {
        /// Offset of the varint's first byte.
        offset: u32,
        /// The type being decoded, such as `i32`.
        target: VarintTarget,
    },
    /// A type code was not one the compact protocol defines in that position.
    #[error("invalid compact type code {code:#x} at byte {offset}")]
    InvalidType {
        /// Offset of the byte holding the code.
        offset: u32,
        /// The 4-bit type code found.
        code: u8,
    },
    /// A field id given as a delta from the previous one overflowed `i16`.
    #[error("field id overflows i16 at byte {offset}")]
    FieldIdOverflow {
        /// Offset of the field header.
        offset: u32,
    },
    /// A container size or binary length exceeded the protocol maximum of `i32::MAX`.
    #[error("size {size} at byte {offset} exceeds the protocol maximum")]
    SizeOverflow {
        /// Offset of the header holding the size.
        offset: u32,
        /// The size found.
        size: u32,
    },
    /// A container size or binary length claimed more data than the input holds.
    #[error("size {size} at byte {offset} cannot fit in the {remaining} bytes remaining")]
    SizeExceedsInput {
        /// Offset of the header holding the size.
        offset: u32,
        /// The size found: bytes for binary values, elements for containers.
        size: u32,
        /// Bytes left in the input after the header.
        remaining: u32,
    },
    /// A bool list element was not `1` (true) or `0`/`2` (false).
    #[error("invalid bool element {byte:#x} at byte {offset}")]
    InvalidBool {
        /// Offset of the byte.
        offset: u32,
        /// The byte found.
        byte: u8,
    },
    /// Containers being skipped nested deeper than [`MAX_SKIP_DEPTH`](super::MAX_SKIP_DEPTH).
    #[error("containers nest deeper than {limit} at byte {offset}")]
    DepthExceeded {
        /// Offset of the container that crossed the limit.
        offset: u32,
        /// The limit, [`MAX_SKIP_DEPTH`](super::MAX_SKIP_DEPTH).
        limit: u16,
    },
}

// Every reader method returns one of these. Keep them from growing unnoticed.
const _: () = assert!(size_of::<ThriftError>() == 16);
const _: () = assert!(size_of::<Result<(), ThriftError>>() == 16);
const _: () = assert!(size_of::<Result<u64, ThriftError>>() == 16);
