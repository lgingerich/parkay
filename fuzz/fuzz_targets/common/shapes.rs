//! The wire type of every field of every structure in a Parquet footer, from
//! `format/parquet.thrift`, and a check that a footer's known fields have them.
//!
//! arrow-rs reads a field as the type it expects whatever its wire type is, and so can
//! lose its place in a footer where parkay skips the field as unknown. The metadata
//! targets compare the two readers only on footers that pass this check.

use parkay::thrift::{CompactReader, ElementType, FieldCursor, FieldType, SKIP_DEPTH_MAX};

/// The wire type of a field or list element, and what is inside it.
#[derive(Clone, Copy)]
pub enum Shape {
    Bool,
    I8,
    I16,
    I32,
    I64,
    Double,
    Binary,
    List(&'static Shape),
    /// A struct, as its known fields: id and shape. Any other field id is unknown to
    /// both readers, which skip it by its wire type.
    Struct(&'static [(i16, Shape)]),
}

use Shape::{Binary, Bool, Double, I8, I16, I32, I64, List, Struct};

const EMPTY: Shape = Struct(&[]);
const TIME_UNIT: Shape = Struct(&[(1, EMPTY), (2, EMPTY), (3, EMPTY)]);
const TIME: Shape = Struct(&[(1, Bool), (2, TIME_UNIT)]);
const LOGICAL_TYPE: Shape = Struct(&[
    (1, EMPTY),
    (2, EMPTY),
    (3, EMPTY),
    (4, EMPTY),
    (5, Struct(&[(1, I32), (2, I32)])),
    (6, EMPTY),
    (7, TIME),
    (8, TIME),
    (10, Struct(&[(1, I8), (2, Bool)])),
    (11, EMPTY),
    (12, EMPTY),
    (13, EMPTY),
    (14, EMPTY),
    (15, EMPTY),
    (16, Struct(&[(1, I8)])),
    (17, Struct(&[(1, Binary)])),
    (18, Struct(&[(1, Binary), (2, I32)])),
    (19, EMPTY),
]);
const SCHEMA_ELEMENT: Shape = Struct(&[
    (1, I32),
    (2, I32),
    (3, I32),
    (4, Binary),
    (5, I32),
    (6, I32),
    (7, I32),
    (8, I32),
    (9, I32),
    (10, LOGICAL_TYPE),
]);
const KEY_VALUE: Shape = Struct(&[(1, Binary), (2, Binary)]);
const STATISTICS: Shape = Struct(&[
    (1, Binary),
    (2, Binary),
    (3, I64),
    (4, I64),
    (5, Binary),
    (6, Binary),
    (7, Bool),
    (8, Bool),
    (9, I64),
]);
const BOUNDING_BOX: Shape = Struct(&[
    (1, Double),
    (2, Double),
    (3, Double),
    (4, Double),
    (5, Double),
    (6, Double),
    (7, Double),
    (8, Double),
]);
const COLUMN_META_DATA: Shape = Struct(&[
    (1, I32),
    (2, List(&I32)),
    (3, List(&Binary)),
    (4, I32),
    (5, I64),
    (6, I64),
    (7, I64),
    (8, List(&KEY_VALUE)),
    (9, I64),
    (10, I64),
    (11, I64),
    (12, STATISTICS),
    (13, List(&Struct(&[(1, I32), (2, I32), (3, I32)]))),
    (14, I64),
    (15, I32),
    (16, Struct(&[(1, I64), (2, List(&I64)), (3, List(&I64))])),
    (17, Struct(&[(1, BOUNDING_BOX), (2, List(&I32))])),
]);
const COLUMN_CHUNK: Shape = Struct(&[
    (1, Binary),
    (2, I64),
    (3, COLUMN_META_DATA),
    (4, I64),
    (5, I32),
    (6, I64),
    (7, I32),
    (
        8,
        Struct(&[(1, EMPTY), (2, Struct(&[(1, List(&Binary)), (2, Binary)]))]),
    ),
    (9, Binary),
]);
const ROW_GROUP: Shape = Struct(&[
    (1, List(&COLUMN_CHUNK)),
    (2, I64),
    (3, I64),
    (4, List(&Struct(&[(1, I32), (2, Bool), (3, Bool)]))),
    (5, I64),
    (6, I64),
    (7, I16),
]);
/// `FileMetaData`. Fields 8 and 9 mean encryption, which parkay rejects, so a footer
/// that has them is never compared and their shapes do not matter.
pub const FILE_META_DATA: Shape = Struct(&[
    (1, I32),
    (2, List(&SCHEMA_ELEMENT)),
    (3, I64),
    (4, List(&ROW_GROUP)),
    (5, List(&KEY_VALUE)),
    (6, Binary),
    (7, List(&Struct(&[(1, EMPTY), (2, EMPTY), (3, EMPTY)]))),
]);

impl Shape {
    /// The element type a list of this shape declares.
    const fn element_type(self) -> ElementType {
        match self {
            Bool => ElementType::Bool,
            I8 => ElementType::I8,
            I16 => ElementType::I16,
            I32 => ElementType::I32,
            I64 => ElementType::I64,
            Double => ElementType::Double,
            Binary => ElementType::Binary,
            List(_) => ElementType::List,
            Struct(_) => ElementType::Struct,
        }
    }

    /// Whether a field of this shape may have the wire type `field_type`.
    fn is_field_type(self, field_type: FieldType) -> bool {
        matches!(
            (self, field_type),
            (Bool, FieldType::Bool(_))
                | (I8, FieldType::I8)
                | (I16, FieldType::I16)
                | (I32, FieldType::I32)
                | (I64, FieldType::I64)
                | (Double, FieldType::Double)
                | (Binary, FieldType::Binary)
                | (List(_), FieldType::List)
                | (Struct(_), FieldType::Struct)
        )
    }
}

/// Whether every known field in the value at the start of `input` has the wire type
/// `shape` gives it. False too if the value cannot be followed or nests too deeply.
pub fn fields_typed(input: &[u8], shape: Shape) -> bool {
    conforms(&mut CompactReader::new(input, 0), shape, 0).unwrap_or(false)
}

fn conforms(reader: &mut CompactReader<'_>, shape: Shape, depth: u16) -> Result<bool, ()> {
    if matches!(shape, Struct(_) | List(_)) && depth >= SKIP_DEPTH_MAX {
        return Err(());
    }
    match shape {
        Struct(known) => {
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(reader).map_err(drop)? {
                let Some((_, expected)) = known.iter().find(|(id, _)| *id == field.id()) else {
                    reader.skip_field(field.field_type()).map_err(drop)?;
                    continue;
                };
                if !expected.is_field_type(field.field_type()) {
                    return Ok(false);
                }
                // A bool field's value is in its header.
                if !matches!(expected, Bool) && !conforms(reader, *expected, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        List(element) => {
            let header = reader.read_list_header().map_err(drop)?;
            if header.size() > 0 && header.element_type() != element.element_type() {
                return Ok(false);
            }
            for _ in 0..header.size() {
                if !conforms(reader, *element, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        scalar => reader
            .skip(scalar.element_type())
            .map(|()| true)
            .map_err(drop),
    }
}
