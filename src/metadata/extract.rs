//! Extraction of column-chunk facts: the one way to read what a footer records for each
//! column chunk.
//!
//! A [`Request`] names which facts are wanted, for which leaf columns and row groups.
//! The results go into a [`ChunkFacts`] the caller owns and reuses from file to file.
//! Chunks are reached through the footer directory, so the cost follows what is asked
//! for and not the width of the table, and only the fields a request needs are decoded.

use std::ops::Range;

use super::errors::{ExtractError, FooterError};
use super::file::Metadata;
use super::types::{Location, Statistics, Wanted};
use super::wire::column_chunk::{Chunk, column_chunk_decode};

/// Which leaf columns a [`Request`] covers.
#[derive(Debug, Clone, Copy)]
pub enum Columns<'a> {
    /// Every leaf column, in the order of the schema.
    All,
    /// The leaf columns with these indices, in this order.
    Indices(&'a [u32]),
}

/// What to extract: which facts, for which columns and row groups.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    columns: Columns<'a>,
    row_group_start: u32,
    row_group_end: u32,
    location: bool,
    statistics: bool,
}

impl<'a> Request<'a> {
    /// A request for `columns` in the row groups from `row_group_start` up to, but not
    /// including, `row_group_end`. It asks for no facts until one of the `with_`
    /// methods adds a group of them.
    #[must_use]
    pub const fn new(columns: Columns<'a>, row_groups: Range<u32>) -> Self {
        Self {
            columns,
            row_group_start: row_groups.start,
            row_group_end: row_groups.end,
            location: false,
            statistics: false,
        }
    }

    /// Also extracts each chunk's [`Location`]: where its bytes are and how they are
    /// encoded.
    #[must_use]
    pub const fn with_location(mut self) -> Self {
        self.location = true;
        self
    }

    /// Also extracts each chunk's [`Statistics`].
    #[must_use]
    pub const fn with_statistics(mut self) -> Self {
        self.statistics = true;
        self
    }

    pub(super) const fn columns(&self) -> Columns<'a> {
        self.columns
    }

    pub(super) const fn row_group_start(&self) -> u32 {
        self.row_group_start
    }

    pub(super) const fn row_group_end(&self) -> u32 {
        self.row_group_end
    }

    /// The fields of `ColumnMetaData` this request needs.
    pub(super) const fn wanted(&self) -> Wanted {
        let mut fields = 0;
        if self.location {
            fields |= Wanted::LOCATION;
        }
        if self.statistics {
            fields |= Wanted::STATISTICS;
        }
        Wanted {
            fields,
            location: self.location,
            statistics: self.statistics,
        }
    }
}

/// A position in a [`ChunkFacts`]: a row group and a column, each counted within the
/// request that filled it, from zero.
///
/// For a request that covers every column and starts at the first row group these are
/// the row group's and the column's indices in the file. The fields are public so that
/// a call names which is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkSlot {
    /// The row group, counted from the first one requested.
    pub row_group: u32,
    /// The column, counted in the order the request lists columns.
    pub column: u32,
}

// The rest of `Statistics` is in `types.rs`, below the decoders. These two read the
// footer through `Metadata`, so they stay above it.
impl Statistics {
    /// The minimum, or a lower bound, where the chunk has one. `metadata` must be the
    /// metadata these statistics were extracted from; with any other, the result is
    /// `None` or unrelated bytes.
    #[must_use]
    pub fn min<'a>(&self, metadata: &'a Metadata) -> Option<&'a [u8]> {
        if self.has(Self::HAS_MIN) {
            metadata.footer_bytes(self.min.offset, self.min.size)
        } else {
            None
        }
    }

    /// The maximum, or an upper bound, where the chunk has one. `metadata` must be the
    /// metadata these statistics were extracted from; with any other, the result is
    /// `None` or unrelated bytes.
    #[must_use]
    pub fn max<'a>(&self, metadata: &'a Metadata) -> Option<&'a [u8]> {
        if self.has(Self::HAS_MAX) {
            metadata.footer_bytes(self.max.offset, self.max.size)
        } else {
            None
        }
    }
}

/// The facts one extraction produced, in storage the caller owns.
///
/// Reuse one value from file to file. It grows only when a request covers more column
/// chunks than any before it, and an extraction into storage that is already large
/// enough allocates nothing.
#[derive(Debug, Default)]
pub struct ChunkFacts {
    row_group_count: u32,
    column_count: u32,
    /// One per chunk, row group by row group. Empty when not requested.
    locations: Vec<Location>,
    /// One per chunk, row group by row group. Empty when not requested.
    statistics: Vec<Statistics>,
}

impl ChunkFacts {
    /// Storage with nothing in it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            row_group_count: 0,
            column_count: 0,
            locations: Vec::new(),
            statistics: Vec::new(),
        }
    }

    /// The number of row groups the last extraction covered.
    #[must_use]
    pub const fn row_group_count(&self) -> u32 {
        self.row_group_count
    }

    /// The number of columns the last extraction covered.
    #[must_use]
    pub const fn column_count(&self) -> u32 {
        self.column_count
    }

    /// The location of the chunk at `slot`. `None` if the last extraction did not ask
    /// for locations or did not cover the slot.
    #[must_use]
    pub fn location(&self, slot: ChunkSlot) -> Option<&Location> {
        self.locations.get(self.index(slot)?)
    }

    /// The statistics of the chunk at `slot`. `None` if the last extraction did not ask
    /// for statistics or did not cover the slot. A chunk the footer records no
    /// statistics for has statistics with nothing in them.
    #[must_use]
    pub fn statistics(&self, slot: ChunkSlot) -> Option<&Statistics> {
        self.statistics.get(self.index(slot)?)
    }

    fn index(&self, slot: ChunkSlot) -> Option<usize> {
        if slot.row_group < self.row_group_count && slot.column < self.column_count {
            let row_group = slot.row_group as usize;
            Some(row_group * self.column_count as usize + slot.column as usize)
        } else {
            None
        }
    }

    /// Empties the storage and sizes it for `chunk_count` chunks of the groups `wanted`
    /// names. Nothing can be read from it until [`Self::complete`] is called, so a
    /// failed extraction leaves no facts behind, old or new.
    pub(super) fn prepare(&mut self, chunk_count: usize, wanted: Wanted) {
        self.row_group_count = 0;
        self.column_count = 0;
        // `resize` writes only the entries it adds. Every entry is written again when
        // its chunk is decoded.
        let location_count = if wanted.location { chunk_count } else { 0 };
        let statistics_count = if wanted.statistics { chunk_count } else { 0 };
        self.locations.resize(location_count, Location::EMPTY);
        self.statistics.resize(statistics_count, Statistics::EMPTY);
    }

    /// Makes the facts readable, once every chunk has been decoded.
    pub(super) const fn complete(&mut self, row_group_count: u32, column_count: u32) {
        self.row_group_count = row_group_count;
        self.column_count = column_count;
    }

    /// Decodes the chunk at `position` in `footer` into the entry at `index`.
    pub(super) fn chunk_decode(
        &mut self,
        index: usize,
        chunk: Chunk<'_>,
        wanted: Wanted,
    ) -> Result<(), FooterError> {
        // A group that was not requested has no storage; its fields are never decoded,
        // and these stand in so the decoder has one shape.
        let mut location_unused = Location::EMPTY;
        let mut statistics_unused = Statistics::EMPTY;
        let location = self
            .locations
            .get_mut(index)
            .unwrap_or(&mut location_unused);
        let statistics = self
            .statistics
            .get_mut(index)
            .unwrap_or(&mut statistics_unused);
        *location = Location::EMPTY;
        *statistics = Statistics::EMPTY;
        column_chunk_decode(chunk, wanted, location, statistics)
    }
}

impl Metadata {
    /// Extracts the facts `request` names into `facts`.
    ///
    /// Only the column chunks the request covers are looked at, and in each only the
    /// fields the request needs. With enough capacity in `facts`, nothing is allocated.
    ///
    /// # Errors
    ///
    /// Returns an error if the request names a row group or a column the file does not
    /// have, or if the metadata of a requested column chunk is malformed, uses something
    /// unsupported, or places the chunk outside the file. `facts` then holds nothing.
    pub fn extract(
        &self,
        request: &Request<'_>,
        facts: &mut ChunkFacts,
    ) -> Result<(), ExtractError> {
        let row_groups = request.row_group_start()..request.row_group_end();
        let row_group_count = self.row_group_count();
        if row_groups.start > row_groups.end || row_groups.end > row_group_count {
            return Err(ExtractError::RowGroups {
                start: row_groups.start,
                end: row_groups.end,
                count: row_group_count,
            });
        }
        let wanted = request.wanted();
        match request.columns() {
            Columns::All => {
                let columns = 0..self.column_count;
                self.extract_columns(row_groups, &columns, self.column_count, wanted, facts)
            }
            Columns::Indices(columns) => {
                if let Some(&column) = columns.iter().find(|&&column| column >= self.column_count) {
                    return Err(ExtractError::Column {
                        column,
                        count: self.column_count,
                    });
                }
                let Ok(column_count) = u32::try_from(columns.len()) else {
                    return Err(ExtractError::Column {
                        column: u32::MAX,
                        count: self.column_count,
                    });
                };
                let columns = columns.iter().copied();
                self.extract_columns(row_groups, &columns, column_count, wanted, facts)
            }
        }
    }

    /// Decodes the chunks of `columns` in `row_groups`, row group by row group, in the
    /// order they are in the footer.
    fn extract_columns(
        &self,
        row_groups: Range<u32>,
        columns: &(impl Iterator<Item = u32> + Clone),
        column_count: u32,
        wanted: Wanted,
        facts: &mut ChunkFacts,
    ) -> Result<(), ExtractError> {
        let row_group_count = row_groups.end - row_groups.start;
        facts.prepare(row_group_count as usize * column_count as usize, wanted);
        let mut index = 0;
        for row_group in row_groups {
            let first = row_group as usize * self.column_count as usize;
            for column in columns.clone() {
                let chunk = Chunk {
                    footer: &self.footer,
                    position: self.chunk_positions[first + column as usize],
                    file_size: self.file_size,
                };
                if let Err(source) = facts.chunk_decode(index, chunk, wanted) {
                    return Err(ExtractError::ColumnChunk {
                        row_group,
                        column,
                        source,
                    });
                }
                index += 1;
            }
        }
        facts.complete(row_group_count, column_count);
        Ok(())
    }
}
