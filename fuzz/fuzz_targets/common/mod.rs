//! Shared helpers for the Thrift and metadata fuzz targets.

#![expect(
    dead_code,
    reason = "each fuzz target uses only a subset of the shared helpers"
)]

pub mod shapes;

use parkay::thrift::{CompactReader, ElementType, FieldCursor, SKIP_DEPTH_MAX};

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

/// Wraps a footer with its little-endian `u32` length and trailing `PAR1` magic.
/// Returns `None` if `footer` is longer than `u32::MAX` bytes.
pub fn footer_tail(footer: &[u8]) -> Option<Vec<u8>> {
    let footer_size = u32::try_from(footer.len()).ok()?;
    let mut tail = Vec::with_capacity(footer.len() + 8);
    tail.extend_from_slice(footer);
    tail.extend_from_slice(&footer_size.to_le_bytes());
    tail.extend_from_slice(b"PAR1");
    Some(tail)
}

const fn is_container(element_type: ElementType) -> bool {
    matches!(
        element_type,
        ElementType::List | ElementType::Set | ElementType::Map | ElementType::Struct
    )
}

/// Decodes one value of type `element_type` with the `read_*` methods (the straightforward path,
/// independent of `skip`'s optimizations), descending like `skip` does.
///
/// Returns `None` if the input is malformed for `read_*`, containers nest at least
/// `SKIP_DEPTH_MAX` deep (where `skip` also stops), or a map is reached (maps have no read API,
/// and delegating them to `skip` would restart `skip`'s depth count).
pub fn walk(reader: &mut CompactReader<'_>, element_type: ElementType, depth: u16) -> Option<()> {
    if is_container(element_type) && depth >= SKIP_DEPTH_MAX {
        return None;
    }
    match element_type {
        ElementType::Bool => drop(reader.read_bool_element().ok()?),
        ElementType::I8 => drop(reader.read_i8().ok()?),
        ElementType::I16 => drop(reader.read_i16().ok()?),
        ElementType::I32 => drop(reader.read_i32().ok()?),
        ElementType::I64 => drop(reader.read_i64().ok()?),
        ElementType::Double => drop(reader.read_double().ok()?),
        ElementType::Binary => drop(reader.read_binary().ok()?),
        ElementType::Uuid => reader.skip(element_type).ok()?,
        ElementType::Map => return None,
        ElementType::List | ElementType::Set => {
            let header = reader.read_list_header().ok()?;
            for _ in 0..header.size() {
                walk(reader, header.element_type(), depth + 1)?;
            }
        }
        ElementType::Struct => {
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(reader).ok()? {
                if let Some(element) = field.field_type().element_type() {
                    walk(reader, element, depth + 1)?;
                }
            }
        }
    }
    Some(())
}

/// Which structural predicate [`check`] enforces while walking containers.
#[derive(Clone, Copy)]
enum Check {
    /// Rejects bool elements (standalone or inside lists/sets) and maps.
    ArrowComparable,
    /// Requires strictly ascending field ids in every struct, and rejects maps.
    FieldsAscend,
}

/// Whether skipping `element_type` from `input` is comparable with arrow-rs's `skip`: false if the
/// value contains a bool element in a list or set, or any map.
///
/// arrow-rs 60.1's `skip` consumes no bytes for bool list elements (each is one byte on
/// the wire), so it diverges on those inputs, and a huge claimed size makes it loop
/// without consuming input. Maps have no read API to look inside, so they are excluded
/// too. Run this before arrow-rs, never after.
pub fn arrow_comparable(input: &[u8], element_type: ElementType) -> bool {
    check(
        &mut CompactReader::new(input, 0),
        element_type,
        0,
        Check::ArrowComparable,
    )
    .unwrap_or(false)
}

/// Whether the field ids of every struct in the value ascend strictly, so that no field
/// repeats and none is out of order. False too if the value cannot be followed, nests
/// too deeply, or holds a map.
///
/// The metadata targets compare parkay with arrow-rs only on such input: the two treat
/// a repeated field differently, and arrow-rs relies on the order of some fields.
pub fn fields_ascend(input: &[u8], element_type: ElementType) -> bool {
    check(
        &mut CompactReader::new(input, 0),
        element_type,
        0,
        Check::FieldsAscend,
    )
    .unwrap_or(false)
}

/// Walks struct fields the way `skip` does (without accumulating `i16` field-id deltas)
/// so `ArrowComparable` does not drop valid structs that both skippers accept.
fn check_struct_for_arrow(reader: &mut CompactReader<'_>, depth: u16) -> Result<bool, ()> {
    loop {
        let byte = reader.read_i8().map_err(drop)?.cast_unsigned();
        let code = byte & 0x0F;
        if code == 0 {
            return Ok(true);
        }
        if byte >> 4 == 0 {
            // Long-form field id: skip the varint without enforcing i16 range.
            reader.skip(ElementType::I64).map_err(drop)?;
        }
        let element = match code {
            1 | 2 => continue,
            3 => ElementType::I8,
            4 => ElementType::I16,
            5 => ElementType::I32,
            6 => ElementType::I64,
            7 => ElementType::Double,
            8 => ElementType::Binary,
            9 => ElementType::List,
            10 => ElementType::Set,
            11 => return Ok(false),
            12 => ElementType::Struct,
            13 => ElementType::Uuid,
            _ => return Err(()),
        };
        if is_container(element) {
            if !check(reader, element, depth + 1, Check::ArrowComparable)? {
                return Ok(false);
            }
        } else {
            reader.skip(element).map_err(drop)?;
        }
    }
}

/// `Ok(false)` if a disqualifying value was found, `Err` if the structure could not be
/// followed; scalars are skipped, so only structure is checked, as in `skip`.
fn check(
    reader: &mut CompactReader<'_>,
    element_type: ElementType,
    depth: u16,
    mode: Check,
) -> Result<bool, ()> {
    if is_container(element_type) && depth >= SKIP_DEPTH_MAX {
        return Err(());
    }
    match element_type {
        ElementType::Map => Ok(false),
        ElementType::Bool if matches!(mode, Check::ArrowComparable) => Ok(false),
        ElementType::List | ElementType::Set => {
            let header = reader.read_list_header().map_err(drop)?;
            if matches!(mode, Check::ArrowComparable)
                && header.size() > 0
                && header.element_type() == ElementType::Bool
            {
                return Ok(false);
            }
            for _ in 0..header.size() {
                if !check(reader, header.element_type(), depth + 1, mode)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ElementType::Struct => match mode {
            Check::ArrowComparable => check_struct_for_arrow(reader, depth),
            Check::FieldsAscend => {
                let mut fields = FieldCursor::default();
                let mut last_id = 0;
                while let Some(field) = fields.next(reader).map_err(drop)? {
                    if field.id() <= last_id {
                        return Ok(false);
                    }
                    last_id = field.id();
                    let Some(element) = field.field_type().element_type() else {
                        continue;
                    };
                    match element {
                        ElementType::List | ElementType::Set | ElementType::Struct => {
                            if !check(reader, element, depth + 1, mode)? {
                                return Ok(false);
                            }
                        }
                        ElementType::Map => return Ok(false),
                        scalar => reader.skip(scalar).map_err(drop)?,
                    }
                }
                Ok(true)
            }
        },
        scalar => reader.skip(scalar).map(|()| true).map_err(drop),
    }
}
