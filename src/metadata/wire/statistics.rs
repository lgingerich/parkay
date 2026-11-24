//! Decoding a column chunk's `Statistics`.

use crate::metadata::errors::{FooterError, Structure};
use crate::metadata::types::{Bytes, Statistics};
use crate::metadata::utils::{Seen, footer_u32, read_u64};
use crate::thrift::{CompactReader, FieldCursor, FieldType};

/// Decodes a `Statistics`.
pub(in crate::metadata) fn statistics_decode(
    reader: &mut CompactReader<'_>,
    statistics: &mut Statistics,
) -> Result<(), FooterError> {
    const STRUCTURE: Structure = Structure::Statistics;
    let mut local = reader.clone();
    // The older bounds are used only when the chunk has neither of the newer ones.
    let mut min_deprecated = Bytes::default();
    let mut max_deprecated = Bytes::default();
    let mut flags = 0_u16;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(&mut local)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Binary) => {
                seen.mark(STRUCTURE, 1, &local)?;
                max_deprecated = bytes_locate(&mut local)?;
            }
            (2, FieldType::Binary) => {
                seen.mark(STRUCTURE, 2, &local)?;
                min_deprecated = bytes_locate(&mut local)?;
            }
            (3, FieldType::I64) => {
                seen.mark(STRUCTURE, 3, &local)?;
                statistics.null_count = read_u64(&mut local, STRUCTURE, 3)?;
                flags |= Statistics::HAS_NULL_COUNT;
            }
            (4, FieldType::I64) => {
                seen.mark(STRUCTURE, 4, &local)?;
                statistics.distinct_count = read_u64(&mut local, STRUCTURE, 4)?;
                flags |= Statistics::HAS_DISTINCT_COUNT;
            }
            (5, FieldType::Binary) => {
                seen.mark(STRUCTURE, 5, &local)?;
                statistics.max = bytes_locate(&mut local)?;
                flags |= Statistics::HAS_MAX;
            }
            (6, FieldType::Binary) => {
                seen.mark(STRUCTURE, 6, &local)?;
                statistics.min = bytes_locate(&mut local)?;
                flags |= Statistics::HAS_MIN;
            }
            (7, FieldType::Bool(is_exact)) => {
                seen.mark(STRUCTURE, 7, &local)?;
                flags |= Statistics::HAS_MAX_EXACT;
                flags |= if is_exact { Statistics::MAX_EXACT } else { 0 };
            }
            (8, FieldType::Bool(is_exact)) => {
                seen.mark(STRUCTURE, 8, &local)?;
                flags |= Statistics::HAS_MIN_EXACT;
                flags |= if is_exact { Statistics::MIN_EXACT } else { 0 };
            }
            (9, FieldType::I64) => {
                seen.mark(STRUCTURE, 9, &local)?;
                statistics.nan_count = read_u64(&mut local, STRUCTURE, 9)?;
                flags |= Statistics::HAS_NAN_COUNT;
            }
            (_, field_type) => local.skip_field(field_type)?,
        }
    }
    if flags & (Statistics::HAS_MIN | Statistics::HAS_MAX) == 0 {
        if seen.contains(2) {
            statistics.min = min_deprecated;
            flags |= Statistics::HAS_MIN | Statistics::BOUNDS_DEPRECATED;
        }
        if seen.contains(1) {
            statistics.max = max_deprecated;
            flags |= Statistics::HAS_MAX | Statistics::BOUNDS_DEPRECATED;
        }
    }
    statistics.flags = flags;
    *reader = local;
    Ok(())
}

/// Reads a binary field and returns where its bytes are in the footer.
fn bytes_locate(reader: &mut CompactReader<'_>) -> Result<Bytes, FooterError> {
    let size = reader.read_binary()?.len();
    let end = reader.position();
    debug_assert!(end >= size);
    Ok(Bytes {
        offset: footer_u32(end - size),
        size: footer_u32(size),
    })
}
