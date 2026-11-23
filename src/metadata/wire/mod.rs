//! Decoders: pure functions from the footer's Thrift bytes to the values the rest of
//! this module keeps. One file for each structure of `format/parquet.thrift`.
//!
//! Nothing here is public. The types callers see are in the files one level up, and
//! hold no Thrift.

pub(super) mod column_chunk;
pub(super) mod file_metadata;
pub(super) mod key_value;
pub(super) mod schema_element;
mod statistics;
