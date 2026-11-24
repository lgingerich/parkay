//! Helpers shared by the footer decoders: which fields of a structure were seen, and
//! reads of the few kinds of value the footer's fields hold.

use super::errors::{FooterError, Structure};
use crate::thrift::CompactReader;

/// The highest field id [`Seen`] can record. Every structure Parkay decodes has fewer
/// fields than this.
const FIELD_ID_MAX: u8 = 31;

/// Records which fields of one structure have been decoded, so that a repeated field
/// and a missing required field can both be reported.
#[derive(Debug, Clone, Copy)]
pub(super) struct Seen(u32);

impl Seen {
    /// Nothing decoded yet.
    pub(super) const fn new() -> Self {
        Self(0)
    }

    /// The given fields decoded, for a structure whose first fields were taken from
    /// somewhere else.
    pub(super) fn of(fields: &[u8]) -> Self {
        let mut seen = Self::new();
        for &field in fields {
            debug_assert!(field >= 1);
            debug_assert!(field <= FIELD_ID_MAX);
            seen.0 |= 1 << field;
        }
        seen
    }

    /// Records that `field` is being decoded.
    ///
    /// A field id that appears twice in one structure is an error: no known writer
    /// produces one, and readers disagree on which copy to keep.
    pub(super) fn mark(
        &mut self,
        structure: Structure,
        field: u8,
        reader: &CompactReader<'_>,
    ) -> Result<(), FooterError> {
        debug_assert!(field >= 1);
        debug_assert!(field <= FIELD_ID_MAX);
        let bit = 1_u32 << field;
        if self.0 & bit != 0 {
            return Err(duplicate_field(structure, field, offset(reader)));
        }
        self.0 |= bit;
        Ok(())
    }

    /// Whether `field` was decoded.
    pub(super) const fn contains(self, field: u8) -> bool {
        debug_assert!(field <= FIELD_ID_MAX);
        self.0 & (1 << field) != 0
    }

    /// Whether every field in `mask`, a set of bits numbered by field id, was decoded.
    pub(super) const fn contains_all(self, mask: u32) -> bool {
        self.0 & mask == mask
    }

    /// Reports `field` as missing unless it was decoded.
    pub(super) const fn require(self, structure: Structure, field: u8) -> Result<(), FooterError> {
        if self.contains(field) {
            Ok(())
        } else {
            Err(missing_field(structure, field))
        }
    }
}

#[cold]
pub(super) const fn duplicate_field(structure: Structure, field: u8, offset: u32) -> FooterError {
    FooterError::DuplicateField {
        structure,
        field,
        offset,
    }
}

#[cold]
pub(super) const fn missing_field(structure: Structure, field: u8) -> FooterError {
    FooterError::MissingField { structure, field }
}

#[cold]
pub(super) const fn invalid_value(structure: Structure, field: u8, offset: u32) -> FooterError {
    FooterError::InvalidValue {
        structure,
        field,
        offset,
    }
}

#[cold]
const fn invalid_utf8(structure: Structure, field: u8, offset: u32) -> FooterError {
    FooterError::InvalidUtf8 {
        structure,
        field,
        offset,
    }
}

/// The reader's position as an offset for an error.
pub(super) fn offset(reader: &CompactReader<'_>) -> u32 {
    footer_u32(reader.position())
}

/// A position or a size within a footer, or a count of things that each took at least
/// a byte of one, as a `u32`. A footer's length is a `u32` in the file, so it fits.
pub(super) fn footer_u32(value: usize) -> u32 {
    debug_assert!(u32::try_from(value).is_ok());
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Reads an `i64` field that holds a count, a size, or an offset, none of which can be
/// negative.
// Note: inlined because extraction reads up to eight of these per column chunk, and out
// of line each returned its result through memory. A plain `#[inline]` was not taken.
// Measured 2% faster on `open_all` for `wide` and `many_row_groups`; the varint decode
// it calls stays out of line, and is most of what remains.
#[inline(always)]
pub(super) fn read_u64(
    reader: &mut CompactReader<'_>,
    structure: Structure,
    field: u8,
) -> Result<u64, FooterError> {
    let offset = offset(reader);
    let value = reader.read_i64()?;
    let Ok(value) = u64::try_from(value) else {
        return Err(invalid_value(structure, field, offset));
    };
    Ok(value)
}

/// Reads an `i32` field that holds a count or a length, which cannot be negative.
pub(super) fn read_u32(
    reader: &mut CompactReader<'_>,
    structure: Structure,
    field: u8,
) -> Result<u32, FooterError> {
    let offset = offset(reader);
    let value = reader.read_i32()?;
    let Ok(value) = u32::try_from(value) else {
        return Err(invalid_value(structure, field, offset));
    };
    Ok(value)
}

/// Reads a string field and checks that it is valid UTF-8.
pub(super) fn read_str<'a>(
    reader: &mut CompactReader<'a>,
    structure: Structure,
    field: u8,
) -> Result<&'a str, FooterError> {
    let offset = offset(reader);
    let bytes = reader.read_binary()?;
    let Ok(text) = str::from_utf8(bytes) else {
        return Err(invalid_utf8(structure, field, offset));
    };
    Ok(text)
}

/// Reads an `i32` field that holds a value of one of the format's enumerations, and
/// converts it with `from_code`. A value the enumeration does not have is an error.
pub(super) fn read_enum<T>(
    reader: &mut CompactReader<'_>,
    structure: Structure,
    field: u8,
    from_code: impl Fn(i32) -> Option<T>,
) -> Result<T, FooterError> {
    let offset = offset(reader);
    let code = reader.read_i32()?;
    let Some(value) = from_code(code) else {
        return Err(invalid_value(structure, field, offset));
    };
    Ok(value)
}
