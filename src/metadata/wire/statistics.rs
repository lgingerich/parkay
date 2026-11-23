//! Decoding a column chunk's `Statistics`.

use crate::metadata::errors::{FooterError, Structure};
use crate::metadata::types::{Bytes, Statistics};
use crate::metadata::utils::{Seen, read_u64};
use crate::thrift::{CompactReader, FieldCursor, FieldType};

/// Decodes a `Statistics`.
pub(in crate::metadata) fn statistics_decode(
    reader: &mut CompactReader<'_>,
    statistics: &mut Statistics,
) -> Result<(), FooterError> {
    const STRUCTURE: Structure = Structure::Statistics;
    // The older bounds are used only when the chunk has neither of the newer ones.
    let mut min_deprecated = None;
    let mut max_deprecated = None;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Binary) => {
                seen.mark(STRUCTURE, 1, reader)?;
                max_deprecated = Some(bytes_locate(reader)?);
            }
            (2, FieldType::Binary) => {
                seen.mark(STRUCTURE, 2, reader)?;
                min_deprecated = Some(bytes_locate(reader)?);
            }
            (3, FieldType::I64) => {
                seen.mark(STRUCTURE, 3, reader)?;
                statistics.null_count = read_u64(reader, STRUCTURE, 3)?;
                statistics.flags |= Statistics::HAS_NULL_COUNT;
            }
            (4, FieldType::I64) => {
                seen.mark(STRUCTURE, 4, reader)?;
                statistics.distinct_count = read_u64(reader, STRUCTURE, 4)?;
                statistics.flags |= Statistics::HAS_DISTINCT_COUNT;
            }
            (5, FieldType::Binary) => {
                seen.mark(STRUCTURE, 5, reader)?;
                statistics.max = bytes_locate(reader)?;
                statistics.flags |= Statistics::HAS_MAX;
            }
            (6, FieldType::Binary) => {
                seen.mark(STRUCTURE, 6, reader)?;
                statistics.min = bytes_locate(reader)?;
                statistics.flags |= Statistics::HAS_MIN;
            }
            (7, FieldType::Bool(is_exact)) => {
                seen.mark(STRUCTURE, 7, reader)?;
                statistics.flags |= Statistics::HAS_MAX_EXACT;
                statistics.flags |= if is_exact { Statistics::MAX_EXACT } else { 0 };
            }
            (8, FieldType::Bool(is_exact)) => {
                seen.mark(STRUCTURE, 8, reader)?;
                statistics.flags |= Statistics::HAS_MIN_EXACT;
                statistics.flags |= if is_exact { Statistics::MIN_EXACT } else { 0 };
            }
            (9, FieldType::I64) => {
                seen.mark(STRUCTURE, 9, reader)?;
                statistics.nan_count = read_u64(reader, STRUCTURE, 9)?;
                statistics.flags |= Statistics::HAS_NAN_COUNT;
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    let has_newer_bound = statistics.has(Statistics::HAS_MIN | Statistics::HAS_MAX);
    if !has_newer_bound {
        if let Some(min) = min_deprecated {
            statistics.min = min;
            statistics.flags |= Statistics::HAS_MIN | Statistics::BOUNDS_DEPRECATED;
        }
        if let Some(max) = max_deprecated {
            statistics.max = max;
            statistics.flags |= Statistics::HAS_MAX | Statistics::BOUNDS_DEPRECATED;
        }
    }
    Ok(())
}

/// Reads a binary field and returns where its bytes are in the footer.
fn bytes_locate(reader: &mut CompactReader<'_>) -> Result<Bytes, FooterError> {
    let size = reader.read_binary()?.len();
    let end = reader.position();
    debug_assert!(end >= size);
    // Positions and sizes within a footer fit a `u32`, because the footer's length does.
    debug_assert!(u32::try_from(end).is_ok());
    Ok(Bytes {
        offset: u32::try_from(end - size).unwrap_or(u32::MAX),
        size: u32::try_from(size).unwrap_or(u32::MAX),
    })
}
