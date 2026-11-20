//! Shared helpers for the Thrift fuzz targets.

// Each target uses only some of these.
#![allow(dead_code)]

use parkay::thrift::{CompactReader, ElementType, FieldCursor, FieldType, MAX_SKIP_DEPTH};

/// The element types a target can start from, chosen by the input's first byte.
pub const TYPES: [ElementType; 12] = [
    ElementType::Struct,
    ElementType::List,
    ElementType::Set,
    ElementType::Map,
    ElementType::Binary,
    ElementType::Bool,
    ElementType::I8,
    ElementType::I16,
    ElementType::I32,
    ElementType::I64,
    ElementType::Double,
    ElementType::Uuid,
];

/// Splits an input into the type chosen by its first byte and the bytes after it.
pub fn split(data: &[u8]) -> Option<(ElementType, &[u8])> {
    let (&selector, input) = data.split_first()?;
    Some((TYPES[usize::from(selector) % TYPES.len()], input))
}

/// Why a walk stopped without decoding the whole value.
pub enum WalkError {
    /// The input is malformed for the `read_*` methods.
    Invalid,
    /// Containers nest at least `MAX_SKIP_DEPTH` deep, where `skip` also stops.
    TooDeep,
    /// A map, which has no read API. Delegating it to `skip` would restart `skip`'s
    /// depth count, so the walk stops and the input is not compared.
    Map,
}

/// Decodes one value of type `ty` with the `read_*` methods (the straightforward path,
/// independent of `skip`'s optimizations), descending like `skip` does.
pub fn walk(r: &mut CompactReader<'_>, ty: ElementType, depth: u16) -> Result<(), WalkError> {
    let invalid = |_| WalkError::Invalid;
    let container = matches!(
        ty,
        ElementType::List | ElementType::Set | ElementType::Map | ElementType::Struct
    );
    if container && depth >= MAX_SKIP_DEPTH {
        return Err(WalkError::TooDeep);
    }
    match ty {
        ElementType::Bool => drop(r.read_bool_element().map_err(invalid)?),
        ElementType::I8 => drop(r.read_i8().map_err(invalid)?),
        ElementType::I16 => drop(r.read_i16().map_err(invalid)?),
        ElementType::I32 => drop(r.read_i32().map_err(invalid)?),
        ElementType::I64 => drop(r.read_i64().map_err(invalid)?),
        ElementType::Double => drop(r.read_double().map_err(invalid)?),
        ElementType::Binary => drop(r.read_binary().map_err(invalid)?),
        ElementType::Uuid => r.skip(ty).map_err(invalid)?,
        ElementType::Map => return Err(WalkError::Map),
        ElementType::List | ElementType::Set => {
            let header = r.read_list_header().map_err(invalid)?;
            for _ in 0..header.size() {
                walk(r, header.element_type(), depth + 1)?;
            }
        }
        ElementType::Struct => {
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(r).map_err(invalid)? {
                let element = match field.ty() {
                    FieldType::Bool(_) => continue,
                    FieldType::I8 => ElementType::I8,
                    FieldType::I16 => ElementType::I16,
                    FieldType::I32 => ElementType::I32,
                    FieldType::I64 => ElementType::I64,
                    FieldType::Double => ElementType::Double,
                    FieldType::Binary => ElementType::Binary,
                    FieldType::List => ElementType::List,
                    FieldType::Set => ElementType::Set,
                    FieldType::Map => ElementType::Map,
                    FieldType::Struct => ElementType::Struct,
                    FieldType::Uuid => ElementType::Uuid,
                };
                walk(r, element, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Whether skipping `ty` from `input` is comparable with arrow-rs's `skip`: false if the
/// value contains a bool element in a list or set, or any map.
///
/// arrow-rs 60.1's `skip` consumes no bytes for bool list elements (each is one byte on
/// the wire), so it diverges on those inputs, and a huge claimed size makes it loop
/// without consuming input. Maps have no read API to look inside, so they are excluded
/// too. Run this before arrow-rs, never after.
pub fn arrow_comparable(input: &[u8], ty: ElementType) -> bool {
    scan(&mut CompactReader::new(input), ty, 0).unwrap_or(false)
}

/// `Ok(false)` if a disqualifying value was found, `Err` if the structure could not be
/// followed; scalars are skipped, so only structure is checked, as in `skip`.
fn scan(r: &mut CompactReader<'_>, ty: ElementType, depth: u16) -> Result<bool, ()> {
    if depth >= MAX_SKIP_DEPTH {
        return Err(());
    }
    match ty {
        ElementType::Bool | ElementType::Map => Ok(false),
        ElementType::List | ElementType::Set => {
            let header = r.read_list_header().map_err(drop)?;
            if header.size() > 0 && header.element_type() == ElementType::Bool {
                return Ok(false);
            }
            for _ in 0..header.size() {
                if !scan(r, header.element_type(), depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ElementType::Struct => {
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(r).map_err(drop)? {
                let element = match field.ty() {
                    FieldType::Bool(_) => continue,
                    FieldType::List => ElementType::List,
                    FieldType::Set => ElementType::Set,
                    FieldType::Map => return Ok(false),
                    FieldType::Struct => ElementType::Struct,
                    _ => {
                        r.skip_field(field.ty()).map_err(drop)?;
                        continue;
                    }
                };
                if !scan(r, element, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        scalar => r.skip(scalar).map(|()| true).map_err(drop),
    }
}
