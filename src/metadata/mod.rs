//! A file's metadata: opening a file from the bytes at its end, its schema, and what its
//! footer records for each column chunk.
//!
//! [`open`] takes any tail of a file and returns its [`Metadata`] and [`Schema`], or the
//! range of bytes still needed. It performs no I/O. What the footer records per column
//! chunk is then read with [`Metadata::extract`], for the columns and row groups a
//! caller names, into a [`ChunkFacts`] the caller owns.
//!
//! # How to read this module
//!
//! The files at this level are what callers see, and hold no Thrift. In the order a
//! file goes through them:
//!
//! 1. `open.rs`: [`open`]. Checks the last eight bytes, finds the footer in the tail,
//!    and hands it to the pass.
//! 2. `wire/file_metadata.rs`: the pass. One walk over the footer that decodes what
//!    belongs to the file as a whole and records where each column chunk starts.
//!    It calls `wire/schema_element.rs` for the schema, and `template/` to get past
//!    the column chunks without parsing most of them.
//! 3. `file.rs`: [`Metadata`], what is kept, and its accessors. `schema.rs`:
//!    [`Schema`].
//! 4. `extract.rs`: [`Metadata::extract`], its request, and [`ChunkFacts`]. It calls
//!    `wire/column_chunk.rs` and `wire/statistics.rs` for each chunk asked for.
//!
//! Beside those: `types.rs` has the format's plain definitions and the values the
//! decoders fill in ([`Location`], [`Statistics`], [`KeyValue`]), so that `wire/`
//! depends on nothing above it. `errors.rs` has the errors, and `utils.rs` what the
//! decoders share.
//!
//! `wire/` is the decoders: functions from the footer's bytes to values, one file for
//! each structure of `format/parquet.thrift`. `template/` is one optimization of the
//! pass, and can be read last or not at all: `compare.rs` is the comparison, which has
//! no state, and `mod.rs` is everything that has: the templates, what they may cost,
//! and the loops.
//!
//! The design is in `docs/subsystems/metadata.md`.

mod errors;
mod extract;
mod file;
mod open;
mod schema;
mod template;
#[cfg(test)]
mod tests;
mod types;
mod utils;
mod wire;

pub use errors::{ExtractError, FooterError, OpenError, SchemaProblem, Structure, Unsupported};
pub use extract::{ChunkFacts, ChunkSlot, Columns, Request};
pub use file::{ColumnOrders, KeyValues, Metadata};
pub use open::{Opened, open};
pub use schema::{Node, Schema};
pub use types::{
    ColumnOrder, CompressionCodec, Encoding, Encodings, KeyValue, Location, LogicalType,
    PhysicalType, Repetition, Statistics, TimeUnit,
};

// A footer error can hold a Thrift error, so the Thrift error types are part of this
// module's API even though the Thrift reader is not.
pub use crate::thrift::{ThriftError, VarintTarget};
