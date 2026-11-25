//! Decoding one `ColumnChunk` and its `ColumnMetaData`: the fields an extraction asks
//! for, and nothing else.

use super::statistics::statistics_decode;
use crate::metadata::errors::{FooterError, Structure, Unsupported};
use crate::metadata::types::{CompressionCodec, ENCODINGS_ALL, Encodings, Location, Statistics};
use crate::metadata::utils::{
    Seen, invalid_value, missing_field, offset, read_enum, read_u32, read_u64,
};
use crate::thrift::{CompactReader, ElementType, FieldCursor, FieldType};

/// Encodings, codec, value count, both sizes, and the data page offset.
const LOCATION_REQUIRED: u32 = (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 9);
/// The required location fields plus the optional dictionary page offset.
const LOCATION_FIELDS: u32 = LOCATION_REQUIRED | (1 << 11);
/// The optional statistics field.
const STATISTICS_FIELDS: u32 = 1 << 12;
/// The optional encoding statistics field.
const DATA_PAGE_ENCODINGS_FIELDS: u32 = 1 << 13;

/// The `PageType` values of data pages in `parquet.thrift`: `DATA_PAGE` and
/// `DATA_PAGE_V2`. The other two, an index page and a dictionary page, are 1 and 2.
const PAGE_TYPE_DATA: i32 = 0;
const PAGE_TYPE_DATA_V2: i32 = 3;
const PAGE_TYPE_MAX: i32 = 3;

/// One column chunk to decode: the footer, where the chunk starts in it, and the size of
/// the file its offsets must fall within.
#[derive(Debug, Clone, Copy)]
pub(in crate::metadata) struct Chunk<'a> {
    pub(in crate::metadata) footer: &'a [u8],
    pub(in crate::metadata) position: u32,
    pub(in crate::metadata) file_size: u64,
}

/// Decodes the requested fields of one `ColumnChunk`, writing directly into `location`
/// and `statistics`.
///
/// Specialized on `WANT_LOCATION`, `WANT_STATISTICS`, and `WANT_DATA_PAGE_ENCODINGS` so
/// unrequested groups compile out and only the fields asked for are decoded. Decoding
/// stops as soon as every wanted field is in hand, so what follows in the chunk is not
/// read and not checked. The data page encodings are written into `location`, so they
/// are wanted only together with it.
#[inline(always)]
pub(in crate::metadata) fn column_chunk_decode<
    const WANT_LOCATION: bool,
    const WANT_STATISTICS: bool,
    const WANT_DATA_PAGE_ENCODINGS: bool,
>(
    chunk: Chunk<'_>,
    location: &mut Location,
    statistics: &mut Statistics,
) -> Result<(), FooterError> {
    const STRUCTURE: Structure = Structure::ColumnChunk;
    const {
        assert!(WANT_LOCATION || !WANT_DATA_PAGE_ENCODINGS);
    }
    if WANT_LOCATION {
        *location = Location::EMPTY;
    }
    if WANT_STATISTICS {
        *statistics = Statistics::EMPTY;
    }
    let mut reader = CompactReader::new(chunk.footer, chunk.position as usize);
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(&mut reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Binary) => {
                return Err(unsupported(Unsupported::ExternalFile, offset(&reader)));
            }
            (3, FieldType::Struct) => {
                seen.mark(STRUCTURE, 3, &reader)?;
                if column_metadata_decode::<WANT_LOCATION, WANT_STATISTICS, WANT_DATA_PAGE_ENCODINGS>(
                    &mut reader,
                    location,
                    statistics,
                )? {
                    break;
                }
            }
            // The crypto metadata of an encrypted column, or its encrypted metadata.
            (8 | 9, _) => {
                return Err(unsupported(Unsupported::Encryption, offset(&reader)));
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 3)?;
    if WANT_LOCATION {
        location_check(location, chunk)?;
    }
    Ok(())
}

/// Decodes the requested fields of one `ColumnMetaData`.
///
/// Returns whether every wanted field was found before the end of the structure, in
/// which case the reader is left part of the way through it.
#[inline(always)]
fn column_metadata_decode<
    const WANT_LOCATION: bool,
    const WANT_STATISTICS: bool,
    const WANT_DATA_PAGE_ENCODINGS: bool,
>(
    reader: &mut CompactReader<'_>,
    location: &mut Location,
    statistics: &mut Statistics,
) -> Result<bool, FooterError> {
    const STRUCTURE: Structure = Structure::ColumnMetaData;
    let wanted_fields = const {
        (if WANT_LOCATION { LOCATION_FIELDS } else { 0 })
            | (if WANT_STATISTICS {
                STATISTICS_FIELDS
            } else {
                0
            })
            | (if WANT_DATA_PAGE_ENCODINGS {
                DATA_PAGE_ENCODINGS_FIELDS
            } else {
                0
            })
    };
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (2, FieldType::List) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 2, reader)?;
                location.encodings = encodings_decode(reader)?;
            }
            // `path_in_schema` is present in every column chunk; skip a binary list
            // directly instead of calling the general nested skipper.
            (3, FieldType::List) => {
                skip_binary_list(reader)?;
                continue;
            }
            (4, FieldType::I32) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 4, reader)?;
                location.codec = read_enum(reader, STRUCTURE, 4, CompressionCodec::from_code)?;
            }
            (5, FieldType::I64) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 5, reader)?;
                location.value_count = read_u64(reader, STRUCTURE, 5)?;
            }
            (6, FieldType::I64) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 6, reader)?;
                location.uncompressed_size = read_u64(reader, STRUCTURE, 6)?;
            }
            (7, FieldType::I64) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 7, reader)?;
                location.compressed_size = read_u64(reader, STRUCTURE, 7)?;
            }
            (9, FieldType::I64) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 9, reader)?;
                location.data_page_offset = read_u64(reader, STRUCTURE, 9)?;
            }
            (11, FieldType::I64) if WANT_LOCATION => {
                seen.mark(STRUCTURE, 11, reader)?;
                location.dictionary_page_offset = read_u64(reader, STRUCTURE, 11)?;
                location.has_dictionary_page_offset = true;
            }
            (12, FieldType::Struct) if WANT_STATISTICS => {
                seen.mark(STRUCTURE, 12, reader)?;
                statistics_decode(reader, statistics)?;
            }
            (13, FieldType::List) if WANT_DATA_PAGE_ENCODINGS => {
                seen.mark(STRUCTURE, 13, reader)?;
                location.data_page_encodings = data_page_encodings_decode(reader)?;
                location.has_data_page_encodings = true;
            }
            (_, field_type) => {
                reader.skip_field(field_type)?;
                continue;
            }
        }
        if wanted_fields != 0 && seen.contains_all(wanted_fields) {
            return Ok(true);
        }
    }
    if WANT_LOCATION && !seen.contains_all(LOCATION_REQUIRED) {
        return Err(missing_location_field(seen));
    }
    Ok(false)
}

#[inline]
fn skip_binary_list(reader: &mut CompactReader<'_>) -> Result<(), FooterError> {
    if reader.try_skip_short_binary_list() {
        return Ok(());
    }
    skip_binary_list_slow(reader)
}

#[cold]
#[inline(never)]
fn skip_binary_list_slow(reader: &mut CompactReader<'_>) -> Result<(), FooterError> {
    let header = reader.read_list_header()?;
    if header.element_type() == ElementType::Binary {
        for _ in 0..header.size() {
            reader.read_binary()?;
        }
    } else {
        for _ in 0..header.size() {
            reader.skip(header.element_type())?;
        }
    }
    Ok(())
}

#[cold]
fn missing_location_field(seen: Seen) -> FooterError {
    for field in [2, 4, 5, 6, 7, 9] {
        if !seen.contains(field) {
            return missing_field(Structure::ColumnMetaData, field);
        }
    }
    unreachable!("missing_location_field called only when a required field is absent")
}

#[cold]
const fn unsupported(feature: Unsupported, offset: u32) -> FooterError {
    FooterError::Unsupported { feature, offset }
}

/// Decodes a list of `Encoding` into a set.
#[inline]
fn encodings_decode(reader: &mut CompactReader<'_>) -> Result<Encodings, FooterError> {
    if let Some(encodings) = reader.try_fold_short_i32_list(0_u16, |acc, code| {
        let bit = 1_u16.checked_shl(code.cast_unsigned()).unwrap_or(0);
        (bit & ENCODINGS_ALL != 0).then_some(acc | bit)
    }) {
        return Ok(Encodings(encodings));
    }
    encodings_decode_slow(reader)
}

/// Slow path for [`encodings_decode`] handling long lists, empty `0x00` lists,
/// padded multi-byte encoding varints, and error reporting.
#[cold]
#[inline(never)]
fn encodings_decode_slow(reader: &mut CompactReader<'_>) -> Result<Encodings, FooterError> {
    let mut local = reader.clone();
    let list_offset = offset(&local);
    let header = local.read_list_header()?;
    if header.size() > 0 && header.element_type() != ElementType::I32 {
        return Err(invalid_value(Structure::ColumnMetaData, 2, list_offset));
    }
    let mut encodings = 0_u16;
    for _ in 0..header.size() {
        let code = local.read_i32()?.cast_unsigned();
        let bit = 1_u16.checked_shl(code).unwrap_or(0);
        if bit & ENCODINGS_ALL == 0 {
            return Err(invalid_value(Structure::ColumnMetaData, 2, list_offset));
        }
        encodings |= bit;
    }
    *reader = local;
    Ok(Encodings(encodings))
}

/// Decodes a list of `PageEncodingStats` into the set of encodings its data pages use.
///
/// An entry counts whatever number of pages it gives, zero included, as in arrow-rs.
/// Entries for dictionary and index pages are checked and left out.
#[inline]
fn data_page_encodings_decode(reader: &mut CompactReader<'_>) -> Result<Encodings, FooterError> {
    let folded = reader.try_fold_short_i32_triple_list(0_u16, |acc, [page_type, code, count]| {
        if count < 0 || !(0..=PAGE_TYPE_MAX).contains(&page_type) {
            return None;
        }
        let bit = 1_u16.checked_shl(code.cast_unsigned()).unwrap_or(0) & ENCODINGS_ALL;
        if bit == 0 {
            return None;
        }
        if page_type == PAGE_TYPE_DATA || page_type == PAGE_TYPE_DATA_V2 {
            Some(acc | bit)
        } else {
            Some(acc)
        }
    });
    if let Some(encodings) = folded {
        return Ok(Encodings(encodings));
    }
    data_page_encodings_decode_slow(reader)
}

/// Slow path for [`data_page_encodings_decode`] handling long lists, entries whose
/// fields are not the three expected ones in order, large page counts, and error
/// reporting.
#[cold]
#[inline(never)]
fn data_page_encodings_decode_slow(
    reader: &mut CompactReader<'_>,
) -> Result<Encodings, FooterError> {
    let list_offset = offset(reader);
    let header = reader.read_list_header()?;
    if header.size() > 0 && header.element_type() != ElementType::Struct {
        return Err(invalid_value(Structure::ColumnMetaData, 13, list_offset));
    }
    let mut encodings = 0_u16;
    // Bounded: the reader checked that `size` structures fit in the input that is left.
    for _ in 0..header.size() {
        encodings |= page_encoding_stats_decode(reader)?;
    }
    Ok(Encodings(encodings))
}

/// Decodes one `PageEncodingStats`. Returns its encoding's bit if it describes data
/// pages, and no bits otherwise.
fn page_encoding_stats_decode(reader: &mut CompactReader<'_>) -> Result<u16, FooterError> {
    const STRUCTURE: Structure = Structure::PageEncodingStats;
    let mut page_type = 0_i32;
    let mut encoding = 0_u16;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::I32) => {
                seen.mark(STRUCTURE, 1, reader)?;
                let value_offset = offset(reader);
                page_type = reader.read_i32()?;
                if !(0..=PAGE_TYPE_MAX).contains(&page_type) {
                    return Err(invalid_value(STRUCTURE, 1, value_offset));
                }
            }
            (2, FieldType::I32) => {
                seen.mark(STRUCTURE, 2, reader)?;
                let value_offset = offset(reader);
                let code = reader.read_i32()?.cast_unsigned();
                encoding = 1_u16.checked_shl(code).unwrap_or(0) & ENCODINGS_ALL;
                if encoding == 0 {
                    return Err(invalid_value(STRUCTURE, 2, value_offset));
                }
            }
            (3, FieldType::I32) => {
                seen.mark(STRUCTURE, 3, reader)?;
                read_u32(reader, STRUCTURE, 3)?;
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    for field in [1, 2, 3] {
        seen.require(STRUCTURE, field)?;
    }
    if page_type == PAGE_TYPE_DATA || page_type == PAGE_TYPE_DATA_V2 {
        Ok(encoding)
    } else {
        Ok(0)
    }
}

/// Checks that a chunk's bytes, as the footer places them, fall within the file.
fn location_check(location: &Location, chunk: Chunk<'_>) -> Result<(), FooterError> {
    if location.data_page_offset > chunk.file_size {
        return Err(invalid_value(Structure::ColumnMetaData, 9, chunk.position));
    }
    let Some(end) = location.start().checked_add(location.compressed_size) else {
        return Err(invalid_value(Structure::ColumnMetaData, 7, chunk.position));
    };
    if end > chunk.file_size {
        return Err(invalid_value(Structure::ColumnMetaData, 7, chunk.position));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::thrift::test_support::{Value, encoded};

    /// One `PageEncodingStats` with fields 1 to 3 in order, as writers emit it.
    fn entry((page_type, encoding, count): (i32, i32, i32)) -> Value {
        Value::Struct(vec![
            (1, Value::I32(page_type)),
            (2, Value::I32(encoding)),
            (3, Value::I32(count)),
        ])
    }

    proptest! {
        /// The fast path and the slow path decode the same set, stop at the same byte,
        /// and reject the same lists.
        #[test]
        fn encoding_stats_fast_path_agrees_with_the_slow_path(
            entries in prop::collection::vec(
                (-1_i32..6, -1_i32..13, prop_oneof![-1_i32..70, 8_000_i32..8_400, any::<i32>()]),
                0..20,
            ),
            trailing in prop::collection::vec(any::<u8>(), 0..4),
        ) {
            let list = Value::List(ElementType::Struct, entries.into_iter().map(entry).collect());
            let mut bytes = encoded(&list);
            bytes.extend(trailing);
            let mut fast = CompactReader::new(&bytes, 0);
            let mut slow = CompactReader::new(&bytes, 0);
            let fast_result = data_page_encodings_decode(&mut fast);
            let slow_result = data_page_encodings_decode_slow(&mut slow);
            prop_assert_eq!(&fast_result, &slow_result);
            if fast_result.is_ok() {
                prop_assert_eq!(fast.position(), slow.position());
            }
        }

        /// Arbitrary bytes never panic, and the two paths still agree.
        #[test]
        fn encoding_stats_of_arbitrary_bytes_agree(bytes in prop::collection::vec(any::<u8>(), 0..40)) {
            let mut fast = CompactReader::new(&bytes, 0);
            let mut slow = CompactReader::new(&bytes, 0);
            let fast_result = data_page_encodings_decode(&mut fast);
            let slow_result = data_page_encodings_decode_slow(&mut slow);
            prop_assert_eq!(&fast_result, &slow_result);
            if fast_result.is_ok() {
                prop_assert_eq!(fast.position(), slow.position());
            }
        }
    }
}
