//! [`Metadata`]: what is kept of one file after it is opened, and the accessors that
//! read it.

use super::errors::{FooterError, Structure};
use super::types::{ColumnOrder, KeyValue};
use super::utils::{footer_u32, invalid_value, read_str};
use super::wire::file_metadata::{Footer, RowGroup, column_order_decode};
use super::wire::key_value::key_value_decode;
use crate::thrift::{CompactReader, ElementType};

/// The metadata of one file: its footer, kept as bytes, and a directory of where each
/// column chunk's metadata is in them.
///
/// What belongs to the file as a whole is decoded when the file is opened and read
/// through the accessors here. What is recorded per column chunk is decoded on request,
/// by [`Metadata::extract`], into storage the caller owns.
///
/// Metadata is immutable and holds no reference to anything else, so it can be kept in a
/// cache and shared between threads, wrapped in whatever the caller shares things with.
#[derive(Debug)]
pub struct Metadata {
    /// The footer's Thrift bytes, copied from the tail.
    pub(super) footer: Box<[u8]>,
    pub(super) file_size: u64,
    pub(super) footer_offset: u64,
    pub(super) version: i32,
    pub(super) row_count: u64,
    pub(super) column_count: u32,
    /// The footer directory: where each column chunk starts in `footer`, row group by
    /// row group, `column_count` entries for each.
    pub(super) chunk_positions: Box<[u32]>,
    pub(super) row_groups: Box<[RowGroup]>,
    /// Where the writer's name starts in `footer`.
    pub(super) created_by: Option<u32>,
    /// Where the list of column orders starts in `footer`.
    pub(super) column_orders: Option<u32>,
    /// Where the list of key-value pairs starts in `footer`.
    pub(super) key_values: Option<u32>,
}

impl Metadata {
    pub(super) fn new(footer: &[u8], decoded: Footer, file_size: u64, footer_offset: u64) -> Self {
        Self {
            footer: footer.into(),
            file_size,
            footer_offset,
            version: decoded.version,
            row_count: decoded.row_count,
            column_count: decoded.column_count,
            chunk_positions: decoded.chunk_positions.into_boxed_slice(),
            row_groups: decoded.row_groups.into_boxed_slice(),
            created_by: decoded.created_by,
            column_orders: decoded.column_orders,
            key_values: decoded.key_value_metadata,
        }
    }

    /// The format version the file states.
    #[must_use]
    pub const fn version(&self) -> i32 {
        self.version
    }

    /// The number of rows in the file, as the footer states it.
    #[must_use]
    pub const fn row_count(&self) -> u64 {
        self.row_count
    }

    /// The number of row groups.
    #[must_use]
    pub fn row_group_count(&self) -> u32 {
        footer_u32(self.row_groups.len())
    }

    /// The number of leaf columns. Every row group has one column chunk for each.
    #[must_use]
    pub const fn column_count(&self) -> u32 {
        self.column_count
    }

    /// The number of rows in a row group. `None` if the file has no such row group.
    #[must_use]
    pub fn row_group_row_count(&self, row_group: u32) -> Option<u64> {
        Some(self.row_groups.get(row_group as usize)?.row_count)
    }

    /// The uncompressed size of a row group's data, as the footer states it. `None` if
    /// the file has no such row group.
    #[must_use]
    pub fn row_group_byte_size(&self, row_group: u32) -> Option<u64> {
        Some(self.row_groups.get(row_group as usize)?.total_byte_size)
    }

    /// The name and version of the writer, where the file states it. Decoded from the
    /// footer on each call.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is not valid UTF-8. Opening a file does not check
    /// it.
    pub fn created_by(&self) -> Result<Option<&str>, FooterError> {
        let Some(position) = self.created_by else {
            return Ok(None);
        };
        let mut reader = CompactReader::new(&self.footer, position as usize);
        read_str(&mut reader, Structure::FileMetaData, 6).map(Some)
    }

    /// How the minimum and maximum of each column's statistics are ordered, one for
    /// each leaf column in order, decoded as they are read. `None` if the file states
    /// no orders.
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not state one order for each leaf column.
    /// Opening a file does not check that.
    pub fn column_orders(&self) -> Result<Option<ColumnOrders<'_>>, FooterError> {
        let Some(position) = self.column_orders else {
            return Ok(None);
        };
        let mut reader = CompactReader::new(&self.footer, position as usize);
        let header = reader.read_list_header()?;
        if header.size() > 0 && header.element_type() != ElementType::Struct {
            return Err(invalid_value(Structure::FileMetaData, 7, position));
        }
        if header.size() != self.column_count {
            return Err(FooterError::ColumnOrderCount {
                found: header.size(),
                expected: self.column_count,
            });
        }
        Ok(Some(ColumnOrders {
            reader,
            remaining: header.size(),
        }))
    }

    /// The file's key-value pairs, decoded as they are read.
    #[must_use]
    pub fn key_value_metadata(&self) -> KeyValues<'_> {
        KeyValues::new(&self.footer, self.key_values)
    }

    /// The size of the file, as given when it was opened.
    #[must_use]
    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Where the footer starts in the file. Add it to an offset in a
    /// [`FooterError`] for a position in the file.
    #[must_use]
    pub const fn footer_offset(&self) -> u64 {
        self.footer_offset
    }

    /// The bytes of memory this metadata holds.
    #[must_use]
    pub fn memory_size(&self) -> usize {
        size_of::<Self>()
            + self.footer.len()
            + self.chunk_positions.len() * size_of::<u32>()
            + self.row_groups.len() * size_of::<RowGroup>()
    }

    /// The bytes of the footer at `offset`, `size` long. `None` if they are not within
    /// this footer, which means the position came from another file's metadata.
    pub(super) fn footer_bytes(&self, offset: u32, size: u32) -> Option<&[u8]> {
        let start = offset as usize;
        let end = start.checked_add(size as usize)?;
        self.footer.get(start..end)
    }
}

/// The column orders of a file, one for each leaf column. See
/// [`Metadata::column_orders`].
///
/// Opening a file passes over the orders without decoding them, so a malformed one is
/// reported here, by the item that reaches it, and ends the iteration.
#[derive(Debug, Clone)]
pub struct ColumnOrders<'a> {
    reader: CompactReader<'a>,
    /// Orders not yet read. Zero once one has failed to decode.
    remaining: u32,
}

impl Iterator for ColumnOrders<'_> {
    type Item = Result<ColumnOrder, FooterError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining = self.remaining.checked_sub(1)?;
        let column_order = column_order_decode(&mut self.reader);
        if column_order.is_err() {
            self.remaining = 0;
        }
        Some(column_order)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.remaining as usize))
    }
}

/// The key-value pairs of a file's metadata, decoded one at a time as they are read.
///
/// Opening a file passes over the pairs without decoding them, so a malformed list
/// header or pair is reported here, by the item that reaches it, and ends the iteration.
#[derive(Debug, Clone)]
pub struct KeyValues<'a> {
    reader: CompactReader<'a>,
    /// Pairs not yet read. Zero once a pair has failed to decode.
    remaining: u32,
    /// Error from the list header (e.g. a non-empty list of non-structs), yielded once
    /// on the first `next()`.
    header_error: Option<FooterError>,
}

impl<'a> KeyValues<'a> {
    fn new(footer: &'a [u8], position: Option<u32>) -> Self {
        let mut pairs = Self {
            reader: CompactReader::new(footer, position.map_or(0, |position| position as usize)),
            remaining: 0,
            header_error: None,
        };
        if let Some(position) = position {
            match pairs.reader.read_list_header() {
                Ok(header) if header.size() == 0 => {}
                Ok(header) if header.element_type() == ElementType::Struct => {
                    pairs.remaining = header.size();
                }
                Ok(_) => {
                    pairs.header_error = Some(invalid_value(Structure::FileMetaData, 5, position));
                }
                Err(error) => {
                    pairs.header_error = Some(FooterError::from(error));
                }
            }
        }
        pairs
    }
}

impl<'a> Iterator for KeyValues<'a> {
    type Item = Result<KeyValue<'a>, FooterError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(error) = self.header_error.take() {
            return Some(Err(error));
        }
        self.remaining = self.remaining.checked_sub(1)?;
        let pair = key_value_decode(&mut self.reader);
        if pair.is_err() {
            self.remaining = 0;
        }
        Some(pair)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let upper = if self.header_error.is_some() {
            1
        } else {
            self.remaining as usize
        };
        (0, Some(upper))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thrift::test_support::{Value, encoded};

    #[test]
    fn key_values_reports_non_empty_non_struct_list() {
        let bytes = encoded(&Value::List(ElementType::I32, vec![Value::I32(1)]));
        let mut iter = KeyValues::new(&bytes, Some(0));
        assert_eq!(iter.size_hint(), (0, Some(1)));
        assert_eq!(
            iter.next(),
            Some(Err(FooterError::InvalidValue {
                structure: Structure::FileMetaData,
                field: 5,
                offset: 0,
            }))
        );
        assert_eq!(iter.next(), None);
        assert_eq!(iter.size_hint(), (0, Some(0)));

        // An empty placeholder list (0x00) is still accepted as empty.
        let mut empty = KeyValues::new(&[0x00], Some(0));
        assert_eq!(empty.next(), None);
    }
}
