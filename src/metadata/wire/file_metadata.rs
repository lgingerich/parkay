//! The pass over a footer that opens a file.
//!
//! Thrift has no length prefixes, so finding where anything in a footer starts means
//! passing over everything before it. This pass does that once. It decodes what belongs
//! to the file as a whole and records where each column chunk begins, without decoding
//! the chunks: that record is the footer directory.

use super::schema_element::schema_decode;
use crate::limits::Limits;
use crate::metadata::errors::{FooterError, Structure, Unsupported};
use crate::metadata::schema::Schema;
use crate::metadata::template::{Templates, chunks_parse};
use crate::metadata::types::ColumnOrder;
use crate::metadata::utils::{Seen, offset, read_u64};
use crate::thrift::{CompactReader, ElementType, Field, FieldCursor, FieldType};

/// What the footer says about one row group as a whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::metadata) struct RowGroup {
    pub(in crate::metadata) row_count: u64,
    pub(in crate::metadata) total_byte_size: u64,
}

impl RowGroup {
    /// The fewest bytes a row group can take in a footer: its three required fields,
    /// each a header and a byte, and its stop marker.
    const ENCODED_SIZE_MIN: usize = 7;
}

/// What the pass found: the facts about the file as a whole, and the directory.
#[derive(Debug)]
pub(in crate::metadata) struct Footer {
    pub(in crate::metadata) version: i32,
    pub(in crate::metadata) row_count: u64,
    /// The schema, unless the footer began with the bytes of the one supplied.
    pub(in crate::metadata) schema: Option<Schema>,
    /// Leaf columns in the schema, and so column chunks in every row group.
    pub(in crate::metadata) column_count: u32,
    /// The directory: where each column chunk starts in the footer, row group by row
    /// group, `column_count` entries for each.
    pub(in crate::metadata) chunk_positions: Vec<u32>,
    pub(in crate::metadata) row_groups: Vec<RowGroup>,
    /// Where the writer's name starts in the footer.
    pub(in crate::metadata) created_by: Option<u32>,
    /// Where the list of column orders starts in the footer.
    pub(in crate::metadata) column_orders: Option<u32>,
    /// Where the list of key-value pairs starts in the footer.
    pub(in crate::metadata) key_value_metadata: Option<u32>,
}

/// Passes over `footer`, the Thrift bytes of a `FileMetaData`.
///
/// `schema_shared` is the schema of an earlier file. If this footer begins with the
/// same bytes that schema was decoded from, those bytes hold the same schema and are
/// not decoded again.
pub(in crate::metadata) fn decode(
    footer: &[u8],
    limits: Limits,
    schema_shared: Option<&Schema>,
) -> Result<Footer, FooterError> {
    // The footer's length is a `u32` in the file, so every position in it fits one.
    debug_assert!(u32::try_from(footer.len()).is_ok());
    let mut pass = Pass::new(footer, limits);
    let mut cursor = pass.resume_after_shared_schema(schema_shared);
    while let Some(field) = cursor.next(&mut pass.reader)? {
        pass.field(field)?;
        pass.field_count += 1;
    }
    // Bytes after the stop marker, inside the declared footer length, are ignored, as
    // every other reader ignores them.
    pass.finish(schema_shared)
}

/// The state of one pass over a footer.
struct Pass<'a> {
    footer: &'a [u8],
    reader: CompactReader<'a>,
    limits: Limits,
    seen: Seen,
    /// Top-level fields read so far, of any kind.
    field_count: u32,
    version: i32,
    row_count: u64,
    schema: Option<Schema>,
    /// Column chunks every row group must have. Set from the schema when the schema
    /// comes first, as every known writer has it, and otherwise from the first row
    /// group, to be checked against the schema at the end.
    chunk_count: Option<u32>,
    chunk_positions: Vec<u32>,
    row_groups: Vec<RowGroup>,
    /// Set when the footer has enough row groups for comparing column chunks with
    /// earlier ones to pay. Otherwise every chunk is parsed.
    templates: Option<Templates>,
    created_by: Option<u32>,
    column_orders: Option<u32>,
    key_value_metadata: Option<u32>,
}

impl<'a> Pass<'a> {
    const STRUCTURE: Structure = Structure::FileMetaData;

    const fn new(footer: &'a [u8], limits: Limits) -> Self {
        Self {
            footer,
            reader: CompactReader::new(footer, 0),
            limits,
            seen: Seen::new(),
            field_count: 0,
            version: 0,
            row_count: 0,
            schema: None,
            chunk_count: None,
            chunk_positions: Vec::new(),
            row_groups: Vec::new(),
            templates: None,
            created_by: None,
            column_orders: None,
            key_value_metadata: None,
        }
    }

    /// Starts the pass after the version and schema when this footer begins with the
    /// bytes `schema_shared` was decoded from. Returns the cursor to read fields with.
    ///
    /// Equal leading bytes decode to the same fields and end at the same place, because
    /// decoding depends only on the bytes read so far.
    fn resume_after_shared_schema(&mut self, schema_shared: Option<&Schema>) -> FieldCursor {
        let Some(schema) = schema_shared else {
            return FieldCursor::default();
        };
        let Some((footer_prefix, version)) = schema.footer_prefix() else {
            return FieldCursor::default();
        };
        if !self.footer.starts_with(footer_prefix) {
            return FieldCursor::default();
        }
        // A prefix is recorded only for a footer that begins with its version, field 1,
        // and then its schema, field 2.
        self.reader = CompactReader::new(self.footer, footer_prefix.len());
        self.seen = Seen::of(&[1, 2]);
        self.field_count = 2;
        self.version = version;
        self.chunk_count = Some(schema.column_count());
        FieldCursor::after(2)
    }

    /// Whether the schema was taken from the one supplied instead of decoded.
    const fn schema_is_shared(&self) -> bool {
        self.schema.is_none() && self.seen.contains(2)
    }

    /// Handles one top-level field of the footer.
    fn field(&mut self, field: Field) -> Result<(), FooterError> {
        const STRUCTURE: Structure = Pass::STRUCTURE;
        match (field.id(), field.field_type()) {
            (1, FieldType::I32) => {
                self.seen.mark(STRUCTURE, 1, &self.reader)?;
                self.version = self.reader.read_i32()?;
            }
            (2, FieldType::List) => {
                self.seen.mark(STRUCTURE, 2, &self.reader)?;
                self.schema_decode()?;
            }
            (3, FieldType::I64) => {
                self.seen.mark(STRUCTURE, 3, &self.reader)?;
                self.row_count = read_u64(&mut self.reader, STRUCTURE, 3)?;
            }
            (4, FieldType::List) => {
                self.seen.mark(STRUCTURE, 4, &self.reader)?;
                self.row_groups_decode()?;
            }
            (5, FieldType::List) => {
                self.seen.mark(STRUCTURE, 5, &self.reader)?;
                self.key_value_metadata = Some(offset(&self.reader));
                self.reader.skip(ElementType::List)?;
            }
            (6, FieldType::Binary) => {
                self.seen.mark(STRUCTURE, 6, &self.reader)?;
                // Note: the writer's name, the column orders, and the key-value pairs
                // are located here and decoded only when a caller asks, from the footer
                // bytes the metadata keeps. Nothing on the path to a scan needs them
                // yet, so decoding and storing them for every file would be work and
                // memory for nothing. If a scan comes to need one of them (column
                // orders, when statistics are used for pruning), give it a decoded
                // form then.
                self.created_by = Some(offset(&self.reader));
                self.reader.skip(ElementType::Binary)?;
            }
            (7, FieldType::List) => {
                self.seen.mark(STRUCTURE, 7, &self.reader)?;
                self.column_orders = Some(offset(&self.reader));
                self.reader.skip(ElementType::List)?;
            }
            // The encryption algorithm, and the key metadata that signs a plaintext
            // footer. Either means the file has encrypted columns.
            (8 | 9, _) => {
                return Err(FooterError::Unsupported {
                    feature: Unsupported::Encryption,
                    offset: offset(&self.reader),
                });
            }
            (_, field_type) => self.reader.skip_field(field_type)?,
        }
        Ok(())
    }

    fn schema_decode(&mut self) -> Result<(), FooterError> {
        // The schema can be recognized by its bytes in a later footer only if this
        // footer began with its version and then its schema.
        let is_shareable = self.field_count == 1 && self.seen.contains(1);
        let mut schema = schema_decode(&mut self.reader, self.limits)?;
        if is_shareable {
            schema.set_footer_prefix(&self.footer[..self.reader.position()], self.version);
        }
        if self.chunk_count.is_none() {
            self.chunk_count = Some(schema.column_count());
        }
        self.schema = Some(schema);
        Ok(())
    }

    fn row_groups_decode(&mut self) -> Result<(), FooterError> {
        let list_offset = offset(&self.reader);
        let header = self.reader.read_list_header()?;
        if header.size() > 0 && header.element_type() != ElementType::Struct {
            return Err(FooterError::InvalidValue {
                structure: Pass::STRUCTURE,
                field: 4,
                offset: list_offset,
            });
        }
        let input_left = self.footer.len() - self.reader.position();
        self.templates = Templates::for_row_groups(input_left, header.size());
        // A row group is at least seven bytes of footer, so this reserves at most a
        // little over two bytes for each byte of input left.
        // Note: both reservations here are exact, not rounded up. The lists are kept as
        // they are in the metadata, and spare room would have to be given back with a
        // second call to the allocator. A fifth of the time to open a small file went
        // to the allocator before this (`docs/perf/open.md`).
        let row_groups_max = input_left / RowGroup::ENCODED_SIZE_MIN;
        let row_group_count = (header.size() as usize).min(row_groups_max);
        self.row_groups.reserve_exact(row_group_count);
        if let Some(chunk_count) = self.chunk_count {
            // A chunk is at least a byte of footer, so this reserves at most four
            // bytes for each byte of input left.
            let chunk_count_max = row_group_count.saturating_mul(chunk_count as usize);
            self.chunk_positions
                .reserve_exact(chunk_count_max.min(input_left));
        } else {
            // The schema comes after the row groups, which no known writer does. The
            // directory then grows as the row groups are read.
        }
        for index in 0..header.size() {
            self.row_group_decode(index)?;
        }
        Ok(())
    }

    fn row_group_decode(&mut self, index: u32) -> Result<(), FooterError> {
        const STRUCTURE: Structure = Structure::RowGroup;
        let mut row_group = RowGroup::default();
        let mut seen = Seen::new();
        let mut cursor = FieldCursor::default();
        while let Some(field) = cursor.next(&mut self.reader)? {
            match (field.id(), field.field_type()) {
                (1, FieldType::List) => {
                    seen.mark(STRUCTURE, 1, &self.reader)?;
                    self.column_chunks_locate(index)?;
                }
                (2, FieldType::I64) => {
                    seen.mark(STRUCTURE, 2, &self.reader)?;
                    row_group.total_byte_size = read_u64(&mut self.reader, STRUCTURE, 2)?;
                }
                (3, FieldType::I64) => {
                    seen.mark(STRUCTURE, 3, &self.reader)?;
                    row_group.row_count = read_u64(&mut self.reader, STRUCTURE, 3)?;
                }
                (_, field_type) => self.reader.skip_field(field_type)?,
            }
        }
        seen.require(STRUCTURE, 1)?;
        seen.require(STRUCTURE, 2)?;
        seen.require(STRUCTURE, 3)?;
        self.row_groups.push(row_group);
        Ok(())
    }

    /// Records where each column chunk of one row group starts, and passes over it.
    fn column_chunks_locate(&mut self, row_group: u32) -> Result<(), FooterError> {
        let list_offset = offset(&self.reader);
        let header = self.reader.read_list_header()?;
        let found = header.size();
        if found > 0 && header.element_type() != ElementType::Struct {
            return Err(FooterError::InvalidValue {
                structure: Structure::RowGroup,
                field: 1,
                offset: list_offset,
            });
        }
        let expected = *self.chunk_count.get_or_insert(found);
        if found != expected {
            return Err(FooterError::ColumnCount {
                row_group,
                found,
                expected,
            });
        }
        // The list's size was checked against the input left, and a chunk is at least a
        // byte, so this reserves at most four bytes for each byte of footer.
        self.chunk_positions.reserve(found as usize);
        let first = self.chunk_positions.len();
        self.chunk_positions.resize(first + found as usize, 0);
        let chunk_positions = &mut self.chunk_positions[first..];
        let start = self.reader.position();
        let end = match &mut self.templates {
            None => chunks_parse(self.footer, start, chunk_positions)?,
            Some(templates) => templates.chunks_locate(self.footer, start, chunk_positions)?,
        };
        self.reader = CompactReader::new(self.footer, end);
        // One entry per column chunk, and every chunk took at least a byte.
        debug_assert!(self.chunk_positions.len() <= self.reader.position());
        Ok(())
    }

    /// Checks what can only be checked once the whole footer has been read.
    fn finish(self, schema_shared: Option<&Schema>) -> Result<Footer, FooterError> {
        const STRUCTURE: Structure = Pass::STRUCTURE;
        self.seen.require(STRUCTURE, 1)?;
        self.seen.require(STRUCTURE, 2)?;
        self.seen.require(STRUCTURE, 3)?;
        self.seen.require(STRUCTURE, 4)?;
        let column_count = match (&self.schema, schema_shared) {
            (Some(schema), _) => schema.column_count(),
            (None, Some(schema)) => {
                debug_assert!(self.schema_is_shared());
                schema.column_count()
            }
            // Field 2 was seen, so the schema was decoded or taken from the one
            // supplied.
            (None, None) => {
                return Err(FooterError::MissingField {
                    structure: STRUCTURE,
                    field: 2,
                });
            }
        };
        // Row groups that came before the schema were checked against each other.
        if let Some(found) = self.chunk_count
            && found != column_count
        {
            return Err(FooterError::ColumnCount {
                row_group: 0,
                found,
                expected: column_count,
            });
        }
        debug_assert_eq!(
            self.chunk_positions.len(),
            self.row_groups.len() * column_count as usize
        );
        Ok(Footer {
            version: self.version,
            row_count: self.row_count,
            schema: self.schema,
            column_count,
            chunk_positions: self.chunk_positions,
            row_groups: self.row_groups,
            created_by: self.created_by,
            column_orders: self.column_orders,
            key_value_metadata: self.key_value_metadata,
        })
    }
}

/// Decodes a `ColumnOrder`, a union of empty structs.
pub(in crate::metadata) fn column_order_decode(
    reader: &mut CompactReader<'_>,
) -> Result<ColumnOrder, FooterError> {
    let union_offset = offset(reader);
    let mut cursor = FieldCursor::default();
    let Some(member) = cursor.next(reader)? else {
        // An order the writer did not state is one this reader cannot use.
        return Ok(ColumnOrder::Unrecognized);
    };
    let column_order = match (member.id(), member.field_type()) {
        (1, FieldType::Struct) => ColumnOrder::TypeDefined,
        (2, FieldType::Struct) => ColumnOrder::Ieee754Total,
        (3, FieldType::Struct) => ColumnOrder::Int96Timestamp,
        _ => ColumnOrder::Unrecognized,
    };
    reader.skip_field(member.field_type())?;
    if cursor.next(reader)?.is_some() {
        return Err(FooterError::InvalidValue {
            structure: Structure::ColumnOrder,
            field: 1,
            offset: union_offset,
        });
    }
    Ok(column_order)
}
