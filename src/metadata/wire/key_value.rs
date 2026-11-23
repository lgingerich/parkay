//! Decoding one `KeyValue` pair of a file's metadata.

use crate::metadata::errors::{FooterError, Structure};
use crate::metadata::types::KeyValue;
use crate::metadata::utils::{Seen, read_str};
use crate::thrift::{CompactReader, FieldCursor, FieldType};

/// Decodes one `KeyValue`.
pub(in crate::metadata) fn key_value_decode<'a>(
    reader: &mut CompactReader<'a>,
) -> Result<KeyValue<'a>, FooterError> {
    const STRUCTURE: Structure = Structure::KeyValue;
    let mut pair = KeyValue {
        key: "",
        value: None,
    };
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Binary) => {
                seen.mark(STRUCTURE, 1, reader)?;
                pair.key = read_str(reader, STRUCTURE, 1)?;
            }
            (2, FieldType::Binary) => {
                seen.mark(STRUCTURE, 2, reader)?;
                pair.value = Some(read_str(reader, STRUCTURE, 2)?);
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 1)?;
    Ok(pair)
}
