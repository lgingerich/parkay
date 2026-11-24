//! Decoding the schema: the footer's flat list of `SchemaElement`, and placing its
//! elements in a tree.

use crate::limits::Limits;
use crate::metadata::errors::{FooterError, SchemaProblem, Structure};
use crate::metadata::schema::{NodeData, Schema};
use crate::metadata::types::{LogicalType, PhysicalType, Repetition, TimeUnit};
use crate::metadata::utils::{
    Seen, footer_u32, invalid_value, offset, read_enum, read_str, read_u32,
};
use crate::thrift::{CompactReader, ElementType, FieldCursor, FieldType};

/// Decodes the schema field of a footer: a list of `SchemaElement`.
pub(in crate::metadata) fn schema_decode(
    reader: &mut CompactReader<'_>,
    limits: Limits,
) -> Result<Schema, FooterError> {
    let list_offset = offset(reader);
    let header = reader.read_list_header()?;
    if header.size() == 0 {
        return Err(FooterError::Schema {
            problem: SchemaProblem::Empty,
            element: 0,
        });
    }
    if header.element_type() != ElementType::Struct {
        return Err(invalid_value(Structure::FileMetaData, 2, list_offset));
    }
    let mut builder = Builder::new(
        limits.schema_depth_max(),
        header.size(),
        reader.remaining_size(),
    );
    for index in 0..header.size() {
        let element = element_decode(reader)?;
        builder.push(index, &element)?;
    }
    builder.finish(header.size())
}

/// One `SchemaElement` as decoded, before it is placed in the tree.
#[derive(Debug, Default)]
struct Element<'a> {
    name: &'a str,
    physical_type: Option<PhysicalType>,
    type_length: Option<u32>,
    repetition: Option<Repetition>,
    child_count: u32,
    /// The equivalent of the element's `ConvertedType`.
    converted_type: Option<LogicalType>,
    scale: Option<i32>,
    precision: Option<i32>,
    field_id: Option<i32>,
    logical_type: Option<LogicalType>,
}

impl Element<'_> {
    /// The element's logical type: its `LogicalType` if it has one, and otherwise the
    /// equivalent of its `ConvertedType`. Writers set both for older readers, and the
    /// newer one says more.
    fn logical_type(&self) -> Option<LogicalType> {
        match (self.logical_type, self.converted_type) {
            (Some(logical_type), _) => Some(logical_type),
            (None, Some(LogicalType::Decimal { .. })) => Some(LogicalType::Decimal {
                scale: self.scale.unwrap_or(0),
                precision: self.precision.unwrap_or(0),
            }),
            (None, converted_type) => converted_type,
        }
    }
}

fn element_decode<'a>(reader: &mut CompactReader<'a>) -> Result<Element<'a>, FooterError> {
    const STRUCTURE: Structure = Structure::SchemaElement;
    let mut element = Element::default();
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::I32) => {
                seen.mark(STRUCTURE, 1, reader)?;
                let code = read_enum(reader, STRUCTURE, 1, PhysicalType::from_code)?;
                element.physical_type = Some(code);
            }
            (2, FieldType::I32) => {
                seen.mark(STRUCTURE, 2, reader)?;
                element.type_length = Some(read_u32(reader, STRUCTURE, 2)?);
            }
            (3, FieldType::I32) => {
                seen.mark(STRUCTURE, 3, reader)?;
                let code = read_enum(reader, STRUCTURE, 3, Repetition::from_code)?;
                element.repetition = Some(code);
            }
            (4, FieldType::Binary) => {
                seen.mark(STRUCTURE, 4, reader)?;
                element.name = read_str(reader, STRUCTURE, 4)?;
            }
            (5, FieldType::I32) => {
                seen.mark(STRUCTURE, 5, reader)?;
                element.child_count = read_u32(reader, STRUCTURE, 5)?;
            }
            (6, FieldType::I32) => {
                seen.mark(STRUCTURE, 6, reader)?;
                let code = read_enum(reader, STRUCTURE, 6, LogicalType::from_converted_code)?;
                element.converted_type = Some(code);
            }
            (7, FieldType::I32) => {
                seen.mark(STRUCTURE, 7, reader)?;
                element.scale = Some(reader.read_i32()?);
            }
            (8, FieldType::I32) => {
                seen.mark(STRUCTURE, 8, reader)?;
                element.precision = Some(reader.read_i32()?);
            }
            (9, FieldType::I32) => {
                seen.mark(STRUCTURE, 9, reader)?;
                element.field_id = Some(reader.read_i32()?);
            }
            (10, FieldType::Struct) => {
                seen.mark(STRUCTURE, 10, reader)?;
                element.logical_type = Some(logical_type_decode(reader)?);
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 4)?;
    Ok(element)
}

/// Decodes a `LogicalType`, a union: a struct with exactly one member.
fn logical_type_decode(reader: &mut CompactReader<'_>) -> Result<LogicalType, FooterError> {
    // What is wrong with the union as a whole is reported against the field of the
    // schema element that holds it.
    let union_offset = offset(reader);
    let mut cursor = FieldCursor::default();
    let Some(member) = cursor.next(reader)? else {
        return Err(invalid_value(Structure::SchemaElement, 10, union_offset));
    };
    let logical_type = match (member.id(), member.field_type()) {
        (5, FieldType::Struct) => decimal_decode(reader)?,
        (7, FieldType::Struct) => {
            let (adjusted_to_utc, unit) = time_decode(reader)?;
            LogicalType::Time {
                adjusted_to_utc,
                unit,
            }
        }
        (8, FieldType::Struct) => {
            let (adjusted_to_utc, unit) = time_decode(reader)?;
            LogicalType::Timestamp {
                adjusted_to_utc,
                unit,
            }
        }
        (10, FieldType::Struct) => integer_decode(reader)?,
        (id, FieldType::Struct) => {
            reader.skip(ElementType::Struct)?;
            LogicalType::from_member_id(id)
        }
        (_, field_type) => {
            reader.skip_field(field_type)?;
            LogicalType::Unrecognized
        }
    };
    if cursor.next(reader)?.is_some() {
        return Err(invalid_value(Structure::SchemaElement, 10, union_offset));
    }
    Ok(logical_type)
}

/// Decodes a `DecimalType`.
fn decimal_decode(reader: &mut CompactReader<'_>) -> Result<LogicalType, FooterError> {
    const STRUCTURE: Structure = Structure::LogicalType;
    let mut scale = 0;
    let mut precision = 0;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::I32) => {
                seen.mark(STRUCTURE, 1, reader)?;
                scale = reader.read_i32()?;
            }
            (2, FieldType::I32) => {
                seen.mark(STRUCTURE, 2, reader)?;
                precision = reader.read_i32()?;
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 1)?;
    seen.require(STRUCTURE, 2)?;
    Ok(LogicalType::Decimal { scale, precision })
}

/// Decodes a `TimeType` or a `TimestampType`, which have the same fields: whether
/// values are normalized to UTC, and their unit.
fn time_decode(reader: &mut CompactReader<'_>) -> Result<(bool, TimeUnit), FooterError> {
    const STRUCTURE: Structure = Structure::LogicalType;
    let mut adjusted_to_utc = false;
    let mut unit = TimeUnit::Milliseconds;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::Bool(value)) => {
                seen.mark(STRUCTURE, 1, reader)?;
                adjusted_to_utc = value;
            }
            (2, FieldType::Struct) => {
                seen.mark(STRUCTURE, 2, reader)?;
                unit = time_unit_decode(reader)?;
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 1)?;
    seen.require(STRUCTURE, 2)?;
    Ok((adjusted_to_utc, unit))
}

/// Decodes a `TimeUnit`, a union of three empty structs.
fn time_unit_decode(reader: &mut CompactReader<'_>) -> Result<TimeUnit, FooterError> {
    let unit_offset = offset(reader);
    let mut cursor = FieldCursor::default();
    let Some(member) = cursor.next(reader)? else {
        return Err(invalid_value(Structure::LogicalType, 2, unit_offset));
    };
    let unit = match (member.id(), member.field_type()) {
        (1, FieldType::Struct) => TimeUnit::Milliseconds,
        (2, FieldType::Struct) => TimeUnit::Microseconds,
        (3, FieldType::Struct) => TimeUnit::Nanoseconds,
        _ => return Err(invalid_value(Structure::LogicalType, 2, unit_offset)),
    };
    reader.skip(ElementType::Struct)?;
    if cursor.next(reader)?.is_some() {
        return Err(invalid_value(Structure::LogicalType, 2, unit_offset));
    }
    Ok(unit)
}

/// Decodes an `IntType`.
fn integer_decode(reader: &mut CompactReader<'_>) -> Result<LogicalType, FooterError> {
    const STRUCTURE: Structure = Structure::LogicalType;
    let mut bit_width = 0;
    let mut signed = false;
    let mut seen = Seen::new();
    let mut cursor = FieldCursor::default();
    while let Some(field) = cursor.next(reader)? {
        match (field.id(), field.field_type()) {
            (1, FieldType::I8) => {
                seen.mark(STRUCTURE, 1, reader)?;
                let value_offset = offset(reader);
                let Ok(value) = u8::try_from(reader.read_i8()?) else {
                    return Err(invalid_value(STRUCTURE, 1, value_offset));
                };
                bit_width = value;
            }
            (2, FieldType::Bool(value)) => {
                seen.mark(STRUCTURE, 2, reader)?;
                signed = value;
            }
            (_, field_type) => reader.skip_field(field_type)?,
        }
    }
    seen.require(STRUCTURE, 1)?;
    seen.require(STRUCTURE, 2)?;
    Ok(LogicalType::Integer { bit_width, signed })
}

/// A group whose children are still being read.
#[derive(Debug, Clone, Copy)]
struct OpenGroup {
    node: u32,
    definition_level: u8,
    repetition_level: u8,
    /// Children not yet read. Never zero while the group is on the stack.
    children_left: u32,
}

/// Places schema elements, arriving in depth-first order, into a tree.
///
/// The tree is built with an explicit stack of the groups still open, so its depth is
/// bounded by the limit and not by the call stack.
#[derive(Debug)]
struct Builder {
    names: String,
    nodes: Vec<NodeData>,
    leaves: Vec<u32>,
    open: Vec<OpenGroup>,
    depth_max: u8,
}

impl Builder {
    /// The fewest bytes a schema element takes in a footer: the header of its name,
    /// the name's length, and its stop marker. A list cannot hold more elements than
    /// the input left divided by this, whatever its header claims.
    const ELEMENT_ENCODED_SIZE_MIN: usize = 3;

    /// The room made for names ahead of time, in bytes for each element.
    // Note: measured, not chosen. Over the 70 schemas of parquet-testing a name is 7.5
    // bytes on average, and the average within one schema is at most 13.3 bytes in
    // nine schemas of ten. With this much room nine schemas in ten store their names
    // without the string growing.
    const NAME_SIZE_RESERVED: usize = 14;

    /// A builder for a schema whose list says it has `element_count` elements, with
    /// `input_left` bytes of footer still unread.
    fn new(depth_max: u8, element_count: u32, input_left: usize) -> Self {
        // Note: room for every element is made once, here, because growing step by
        // step is a call to the allocator at each step. It is bounded by the input,
        // not by a fixed number: at most what the largest schema that could follow
        // would use.
        let reserved = (element_count as usize).min(input_left / Self::ELEMENT_ENCODED_SIZE_MIN);
        Self {
            names: String::with_capacity(reserved * Self::NAME_SIZE_RESERVED),
            nodes: Vec::with_capacity(reserved),
            // Note: one entry too many for each group, since only leaves go here.
            // Giving that back would cost a call to the allocator for a few bytes.
            leaves: Vec::with_capacity(reserved),
            // Note: allocates when the root is pushed, and is freed when the schema is
            // built. An array on the stack would have to hold the limit's 255 levels.
            open: Vec::new(),
            depth_max,
        }
    }

    /// Adds the element at `index` under the innermost open group.
    fn push(&mut self, index: u32, element: &Element<'_>) -> Result<(), FooterError> {
        let depth = self.open.len();
        if depth > usize::from(self.depth_max) {
            return Err(FooterError::SchemaTooDeep {
                element: index,
                limit: self.depth_max,
            });
        }
        let parent = if let Some(parent) = self.open.last_mut() {
            debug_assert!(parent.children_left > 0);
            parent.children_left -= 1;
            Some(*parent)
        } else if index == 0 {
            None
        } else {
            return Err(FooterError::Schema {
                problem: SchemaProblem::ExtraRoot,
                element: index,
            });
        };
        let node = node_place(index, element, parent)?;
        if node.physical_type.is_some() {
            self.leaves.push(index);
        }
        self.names.push_str(element.name);
        self.nodes.push(NodeData {
            name_end: footer_u32(self.names.len()),
            ..node
        });
        if node.child_count > 0 {
            self.open.push(OpenGroup {
                node: index,
                definition_level: node.definition_level,
                repetition_level: node.repetition_level,
                children_left: node.child_count,
            });
        }
        while self
            .open
            .last()
            .is_some_and(|group| group.children_left == 0)
        {
            self.open.pop();
        }
        Ok(())
    }

    /// Checks that every group got its children, and returns the schema.
    fn finish(self, element_count: u32) -> Result<Schema, FooterError> {
        if !self.open.is_empty() {
            return Err(FooterError::Schema {
                problem: SchemaProblem::ChildrenMissing,
                element: element_count,
            });
        }
        debug_assert_eq!(self.nodes.len(), element_count as usize);
        // Note: the room left over in `names` is kept. Giving it back is a call to the
        // allocator, and opening the 70 small files was about 7% slower with it
        // (`docs/perf/open.md`). It is at most `NAME_SIZE_RESERVED` bytes an element.
        Ok(Schema {
            names: self.names,
            nodes: self.nodes,
            leaves: self.leaves,
            footer_prefix: Vec::new(),
            version: 0,
        })
    }
}

/// Works out what the element at `index` is in the tree, given its parent: the root, a
/// group, or a leaf column, and its levels. The name is filled in by the caller.
fn node_place(
    index: u32,
    element: &Element<'_>,
    parent: Option<OpenGroup>,
) -> Result<NodeData, FooterError> {
    let problem = |problem| FooterError::Schema {
        problem,
        element: index,
    };
    let Some(parent) = parent else {
        // The root is always a group and has no repetition, whatever the file says:
        // some writers give it one, and some give an empty schema a root with no
        // children.
        return Ok(NodeData {
            name_end: 0,
            parent: 0,
            child_count: element.child_count,
            type_length: None,
            field_id: element.field_id,
            logical_type: element.logical_type(),
            physical_type: None,
            repetition: None,
            definition_level: 0,
            repetition_level: 0,
        });
    };
    let is_group = element.child_count > 0;
    // Some writers set no children, or zero, on a field with no type: an empty group.
    let physical_type = if is_group {
        None
    } else {
        element.physical_type
    };
    // The format requires a repetition on every field but the root. Requiring it here
    // also means a repetition written with the wrong wire type, which is skipped as
    // unknown, is an error and not a silently different level.
    let Some(repetition) = element.repetition else {
        return Err(problem(SchemaProblem::RepetitionMissing));
    };
    if physical_type == Some(PhysicalType::FixedLenByteArray) && element.type_length.is_none() {
        return Err(problem(SchemaProblem::LengthMissing));
    }
    // The depth limit keeps both levels within a byte.
    let (defined, repeated) = match repetition {
        Repetition::Required => (0, 0),
        Repetition::Optional => (1, 0),
        Repetition::Repeated => (1, 1),
    };
    Ok(NodeData {
        name_end: 0,
        parent: parent.node,
        child_count: element.child_count,
        type_length: element.type_length,
        field_id: element.field_id,
        logical_type: element.logical_type(),
        physical_type,
        repetition: Some(repetition),
        definition_level: parent.definition_level + defined,
        repetition_level: parent.repetition_level + repeated,
    })
}
