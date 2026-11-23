//! Decoding one `ColumnChunk` and its `ColumnMetaData`: the fields an extraction asks
//! for, and nothing else.

use super::statistics::statistics_decode;
use crate::metadata::errors::{FooterError, Structure, Unsupported};
use crate::metadata::types::{
    CompressionCodec, ENCODINGS_ALL, Encodings, Location, Statistics, Wanted,
};
use crate::metadata::utils::{Seen, offset, read_enum, read_u64};
use crate::thrift::{CompactReader, ElementType, FieldCursor, FieldType};

/// One column chunk to decode: the footer, where the chunk starts in it, and the size of
/// the file its offsets must fall within.
#[derive(Debug, Clone, Copy)]
pub(in crate::metadata) struct Chunk<'a> {
    pub(in crate::metadata) footer: &'a [u8],
    pub(in crate::metadata) position: u32,
    pub(in crate::metadata) file_size: u64,
}

/// Decodes the fields of one `ColumnChunk` that `wanted` names.
///
/// Decoding stops as soon as everything wanted is in hand, so what follows in the
/// chunk is not read and not checked.
pub(in crate::metadata) fn column_chunk_decode(
    chunk: Chunk<'_>,
    wanted: Wanted,
    location: &mut Location,
    statistics: &mut Statistics,
) -> Result<(), FooterError> {
    const STRUCTURE: Structure = Structure::ColumnChunk;
    let mut reader = CompactReader::new(chunk.footer, chunk.position as usize);
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(&mut reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Binary) => {
                return Err(FooterError::Unsupported {
                    feature: Unsupported::ExternalFile,
                    offset: offset(&reader),
                });
            }
            (3, FieldType::Struct) => {
                seen.mark(STRUCTURE, 3, &reader)?;
                let is_complete =
                    column_metadata_decode(&mut reader, wanted, location, statistics)?;
                if is_complete {
                    break;
                }
            }
            // The crypto metadata of an encrypted column, or its encrypted metadata.
            (8 | 9, _) => {
                return Err(FooterError::Unsupported {
                    feature: Unsupported::Encryption,
                    offset: offset(&reader),
                });
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 3)?;
    if wanted.location {
        location_check(location, chunk)?;
    }
    Ok(())
}

/// Decodes the fields of one `ColumnMetaData` that `wanted` names.
///
/// Returns whether every wanted field was found before the end of the structure, in
/// which case the reader is left part of the way through it.
fn column_metadata_decode(
    reader: &mut CompactReader<'_>,
    wanted: Wanted,
    location: &mut Location,
    statistics: &mut Statistics,
) -> Result<bool, FooterError> {
    const STRUCTURE: Structure = Structure::ColumnMetaData;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        let id = field.id();
        let is_wanted = (0..=Wanted::FIELD_ID_MAX).contains(&id) && wanted.fields & (1 << id) != 0;
        if !is_wanted {
            reader.skip_field(field.field_type())?;
            continue;
        }
        match (id, field.field_type()) {
            (2, FieldType::List) => {
                seen.mark(STRUCTURE, 2, reader)?;
                location.encodings = encodings_decode(reader)?;
            }
            (4, FieldType::I32) => {
                seen.mark(STRUCTURE, 4, reader)?;
                location.codec = read_enum(reader, STRUCTURE, 4, CompressionCodec::from_code)?;
            }
            (5, FieldType::I64) => {
                seen.mark(STRUCTURE, 5, reader)?;
                location.value_count = read_u64(reader, STRUCTURE, 5)?;
            }
            (6, FieldType::I64) => {
                seen.mark(STRUCTURE, 6, reader)?;
                location.uncompressed_size = read_u64(reader, STRUCTURE, 6)?;
            }
            (7, FieldType::I64) => {
                seen.mark(STRUCTURE, 7, reader)?;
                location.compressed_size = read_u64(reader, STRUCTURE, 7)?;
            }
            (9, FieldType::I64) => {
                seen.mark(STRUCTURE, 9, reader)?;
                location.data_page_offset = read_u64(reader, STRUCTURE, 9)?;
            }
            (11, FieldType::I64) => {
                seen.mark(STRUCTURE, 11, reader)?;
                location.dictionary_page_offset = read_u64(reader, STRUCTURE, 11)?;
                location.has_dictionary_page_offset = true;
            }
            (12, FieldType::Struct) => {
                seen.mark(STRUCTURE, 12, reader)?;
                statistics_decode(reader, statistics)?;
            }
            // A wanted field with a wire type it cannot have is skipped as if unknown.
            (_, field_type) => reader.skip_field(field_type)?,
        }
        if seen.contains_all(wanted.fields) {
            return Ok(true);
        }
    }
    if wanted.location {
        for field in [2, 4, 5, 6, 7, 9] {
            seen.require(STRUCTURE, field)?;
        }
    }
    Ok(false)
}

/// Decodes a list of `Encoding` into a set.
fn encodings_decode(reader: &mut CompactReader<'_>) -> Result<Encodings, FooterError> {
    let invalid = FooterError::InvalidValue {
        structure: Structure::ColumnMetaData,
        field: 2,
        offset: offset(reader),
    };
    let header = reader.read_list_header()?;
    if header.size() > 0 && header.element_type() != ElementType::I32 {
        return Err(invalid);
    }
    let mut encodings = 0_u16;
    for _ in 0..header.size() {
        let code = reader.read_i32()?;
        let Some(bit) = u32::try_from(code)
            .ok()
            .and_then(|code| 1_u16.checked_shl(code))
        else {
            return Err(invalid);
        };
        if bit & ENCODINGS_ALL == 0 {
            return Err(invalid);
        }
        encodings |= bit;
    }
    Ok(Encodings(encodings))
}

/// Checks that a chunk's bytes, as the footer places them, fall within the file.
fn location_check(location: &Location, chunk: Chunk<'_>) -> Result<(), FooterError> {
    let invalid = |field| FooterError::InvalidValue {
        structure: Structure::ColumnMetaData,
        field,
        offset: chunk.position,
    };
    if location.data_page_offset > chunk.file_size {
        return Err(invalid(9));
    }
    let Some(end) = location.start().checked_add(location.compressed_size) else {
        return Err(invalid(7));
    };
    if end > chunk.file_size {
        return Err(invalid(7));
    }
    Ok(())
}
