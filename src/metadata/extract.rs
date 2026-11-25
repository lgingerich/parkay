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
use super::types::{Location, Statistics};
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
    data_page_encodings: bool,
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
            data_page_encodings: false,
        }
    }

    /// Also extracts each chunk's [`Location`]: where its bytes are and how they are
    /// encoded.
    #[must_use]
    pub const fn with_location(mut self) -> Self {
        self.location = true;
        self
    }

    /// Also extracts each chunk's [`Location`] with the encodings of its data pages
    /// filled in ([`Location::data_page_encodings`]), which say whether a chunk is
    /// dictionary-encoded throughout.
    ///
    /// This is a request of its own because the footer records these after a chunk's
    /// statistics: reading them means passing over the statistics of every chunk, which
    /// a request for locations alone never touches.
    #[must_use]
    pub const fn with_data_page_encodings(mut self) -> Self {
        self.location = true;
        self.data_page_encodings = true;
        self
    }

    /// Also extracts each chunk's [`Statistics`].
    #[must_use]
    pub const fn with_statistics(mut self) -> Self {
        self.statistics = true;
        self
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
            (slot.row_group as usize)
                .checked_mul(self.column_count as usize)?
                .checked_add(slot.column as usize)
        } else {
            None
        }
    }

    /// Hides any facts from a prior extraction so any error leaves nothing readable.
    const fn clear(&mut self) {
        self.row_group_count = 0;
        self.column_count = 0;
    }

    /// Empties the storage and sizes it for `chunk_count` chunks of the requested
    /// groups. Nothing can be read from it until [`Self::complete`] is called, so a
    /// failed extraction leaves no facts behind, old or new.
    fn prepare(&mut self, chunk_count: usize, want_location: bool, want_statistics: bool) {
        self.clear();
        // `resize` writes only the entries it adds. Every entry is overwritten when its
        // chunk is decoded.
        self.locations
            .resize(if want_location { chunk_count } else { 0 }, Location::EMPTY);
        self.statistics.resize(
            if want_statistics { chunk_count } else { 0 },
            Statistics::EMPTY,
        );
    }

    /// Makes the facts readable, once every chunk has been decoded.
    const fn complete(&mut self, row_group_count: u32, column_count: u32) {
        self.row_group_count = row_group_count;
        self.column_count = column_count;
    }
}

#[cold]
#[inline(never)]
fn chunk_error_all(
    row_group_start: u32,
    column_count: u32,
    index: usize,
    source: FooterError,
) -> ExtractError {
    let columns = column_count as usize;
    let row_group_offset = u32::try_from(index / columns).unwrap_or(u32::MAX);
    let column = u32::try_from(index % columns).unwrap_or(u32::MAX);
    ExtractError::ColumnChunk {
        row_group: row_group_start.saturating_add(row_group_offset),
        column,
        source,
    }
}

#[cold]
#[inline(never)]
const fn chunk_error(row_group: u32, column: u32, source: FooterError) -> ExtractError {
    ExtractError::ColumnChunk {
        row_group,
        column,
        source,
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
        facts.clear();
        let row_groups = request.row_group_start..request.row_group_end;
        let row_group_count = self.row_group_count();
        if row_groups.start > row_groups.end || row_groups.end > row_group_count {
            return Err(ExtractError::RowGroups {
                start: row_groups.start,
                end: row_groups.end,
                count: row_group_count,
            });
        }
        let column_count = match request.columns {
            Columns::All => self.column_count,
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
                column_count
            }
        };
        let columns = request.columns;
        // `with_data_page_encodings` also asks for the location, so the encodings are
        // never wanted without it.
        debug_assert!(request.location || !request.data_page_encodings);
        match (
            request.location,
            request.statistics,
            request.data_page_encodings,
        ) {
            (true, true, true) => {
                self.extract_mode::<true, true, true>(row_groups, columns, column_count, facts)
            }
            (true, true, false) => {
                self.extract_mode::<true, true, false>(row_groups, columns, column_count, facts)
            }
            (true, false, true) => {
                self.extract_mode::<true, false, true>(row_groups, columns, column_count, facts)
            }
            (true, false, false) => {
                self.extract_mode::<true, false, false>(row_groups, columns, column_count, facts)
            }
            (false, true, _) => {
                self.extract_mode::<false, true, false>(row_groups, columns, column_count, facts)
            }
            (false, false, _) => {
                self.extract_mode::<false, false, false>(row_groups, columns, column_count, facts)
            }
        }
    }

    /// Decodes the requested chunks, specialized by which fact groups are wanted.
    fn extract_mode<
        const WANT_LOCATION: bool,
        const WANT_STATISTICS: bool,
        const WANT_DATA_PAGE_ENCODINGS: bool,
    >(
        &self,
        row_groups: Range<u32>,
        columns: Columns<'_>,
        column_count: u32,
        facts: &mut ChunkFacts,
    ) -> Result<(), ExtractError> {
        let row_group_count = row_groups.end - row_groups.start;
        let chunk_count = (row_group_count as usize)
            .checked_mul(column_count as usize)
            .ok_or(ExtractError::Column {
                column: u32::MAX,
                count: self.column_count,
            })?;
        facts.prepare(chunk_count, WANT_LOCATION, WANT_STATISTICS);
        match columns {
            Columns::All => {
                self.extract_all_mode::<WANT_LOCATION, WANT_STATISTICS, WANT_DATA_PAGE_ENCODINGS>(
                    row_groups.start,
                    chunk_count,
                    facts,
                )?;
            }
            Columns::Indices(indices) => {
                self.extract_indices_mode::<WANT_LOCATION, WANT_STATISTICS, WANT_DATA_PAGE_ENCODINGS>(
                    row_groups,
                    indices,
                    chunk_count,
                    facts,
                )?;
            }
        }
        facts.complete(row_group_count, column_count);
        Ok(())
    }

    /// Decodes a contiguous run of all-columns chunks without per-chunk closure calls or
    /// bounds checks on `facts`.
    #[inline(never)]
    fn extract_all_mode<
        const WANT_LOCATION: bool,
        const WANT_STATISTICS: bool,
        const WANT_DATA_PAGE_ENCODINGS: bool,
    >(
        &self,
        row_group_start: u32,
        chunk_count: usize,
        facts: &mut ChunkFacts,
    ) -> Result<(), ExtractError> {
        let first_chunk = row_group_start as usize * self.column_count as usize;
        let positions = &self.chunk_positions[first_chunk..first_chunk + chunk_count];
        let locations = if WANT_LOCATION {
            &mut facts.locations[..chunk_count]
        } else {
            &mut []
        };
        let statistics = if WANT_STATISTICS {
            &mut facts.statistics[..chunk_count]
        } else {
            &mut []
        };
        let mut location_unused = Location::EMPTY;
        let mut statistics_unused = Statistics::EMPTY;
        for (index, &position) in positions.iter().enumerate() {
            let location = if WANT_LOCATION {
                &mut locations[index]
            } else {
                &mut location_unused
            };
            let stats = if WANT_STATISTICS {
                &mut statistics[index]
            } else {
                &mut statistics_unused
            };
            let chunk = Chunk {
                footer: &self.footer,
                position,
                file_size: self.file_size,
            };
            if let Err(source) =
                column_chunk_decode::<WANT_LOCATION, WANT_STATISTICS, WANT_DATA_PAGE_ENCODINGS>(
                    chunk, location, stats,
                )
            {
                return Err(chunk_error_all(
                    row_group_start,
                    self.column_count,
                    index,
                    source,
                ));
            }
        }
        Ok(())
    }

    /// Decodes selected column indices across `row_groups`.
    #[inline(never)]
    fn extract_indices_mode<
        const WANT_LOCATION: bool,
        const WANT_STATISTICS: bool,
        const WANT_DATA_PAGE_ENCODINGS: bool,
    >(
        &self,
        row_groups: Range<u32>,
        indices: &[u32],
        chunk_count: usize,
        facts: &mut ChunkFacts,
    ) -> Result<(), ExtractError> {
        let file_columns = self.column_count as usize;
        let locations = if WANT_LOCATION {
            &mut facts.locations[..chunk_count]
        } else {
            &mut []
        };
        let statistics = if WANT_STATISTICS {
            &mut facts.statistics[..chunk_count]
        } else {
            &mut []
        };
        let mut location_unused = Location::EMPTY;
        let mut statistics_unused = Statistics::EMPTY;
        let mut index = 0;
        for row_group in row_groups {
            let first = row_group as usize * file_columns;
            let positions = &self.chunk_positions[first..first + file_columns];
            for &column in indices {
                let location = if WANT_LOCATION {
                    &mut locations[index]
                } else {
                    &mut location_unused
                };
                let stats = if WANT_STATISTICS {
                    &mut statistics[index]
                } else {
                    &mut statistics_unused
                };
                let chunk = Chunk {
                    footer: &self.footer,
                    position: positions[column as usize],
                    file_size: self.file_size,
                };
                index += 1;
                if let Err(source) =
                    column_chunk_decode::<WANT_LOCATION, WANT_STATISTICS, WANT_DATA_PAGE_ENCODINGS>(
                        chunk, location, stats,
                    )
                {
                    return Err(chunk_error(row_group, column, source));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::wire::file_metadata::RowGroup;

    #[test]
    fn extract_clears_previous_facts_on_request_validation_error() {
        let metadata = Metadata {
            footer: Box::new([]),
            file_size: 1024,
            footer_offset: 1016,
            version: 1,
            row_count: 0,
            column_count: 1,
            chunk_positions: Box::new([0]),
            row_groups: Box::new([RowGroup::default()]),
            created_by: None,
            column_orders: None,
            key_values: None,
        };
        let mut facts = ChunkFacts::new();
        facts.prepare(1, true, true);
        facts.complete(1, 1);
        assert!(
            facts
                .location(ChunkSlot {
                    row_group: 0,
                    column: 0
                })
                .is_some()
        );

        let bad_row_groups = Request::new(Columns::All, 0..2).with_location();
        assert_eq!(
            metadata.extract(&bad_row_groups, &mut facts),
            Err(ExtractError::RowGroups {
                start: 0,
                end: 2,
                count: 1,
            })
        );
        assert_eq!(facts.row_group_count(), 0);
        assert_eq!(facts.column_count(), 0);
        assert!(
            facts
                .location(ChunkSlot {
                    row_group: 0,
                    column: 0
                })
                .is_none()
        );

        facts.prepare(1, true, false);
        facts.complete(1, 1);
        let bad_column = Request::new(Columns::Indices(&[1]), 0..1).with_location();
        assert_eq!(
            metadata.extract(&bad_column, &mut facts),
            Err(ExtractError::Column {
                column: 1,
                count: 1,
            })
        );
        assert_eq!(facts.row_group_count(), 0);
        assert_eq!(facts.column_count(), 0);
        assert!(
            facts
                .location(ChunkSlot {
                    row_group: 0,
                    column: 0
                })
                .is_none()
        );
    }
}
