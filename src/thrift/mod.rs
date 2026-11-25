//! Decoding of the Thrift compact protocol, which Parquet uses for its footer and
//! page headers.
//!
//! This layer knows nothing about Parquet's own structs; those decoders are built on
//! top of it. It reads from a borrowed buffer and returns an error on any malformed
//! input. It allocates nothing, except that [`CompactReader::skip_marking`] grows the
//! [`Marks`] its caller owns.
//!
//! Every length the reader returns has been checked against the remaining input, so a
//! caller may allocate in proportion to it. The only unbounded structure an attacker
//! controls is the nesting of unknown fields, and [`CompactReader::skip`] limits that.
//!
//! This module imports nothing else from the crate, so it can move to its own crate
//! unchanged if that is ever needed.
//!
//! # How to read this module
//!
//! - `types.rs`: public wire types ([`ElementType`], [`FieldType`], [`Field`],
//!   [`ListHeader`](types::ListHeader)) and compact-protocol type-code constants.
//! - `reader.rs`: [`FieldCursor`] and [`CompactReader`]'s value-reading methods.
//! - `varint.rs`: unsigned LEB128 varint, zigzag decoding, and container-size decoding.
//! - `marks.rs`: [`Marks`] and the windowed marker abstractions used when skipping.
//! - `skip/`: fast windowed skipping (`skip/mod.rs`) and cold error replay / fallbacks
//!   (`skip/cold.rs`).
//! - `errors.rs`: [`ThriftError`] and [`VarintTarget`].
//!
//! Reference: the Thrift compact protocol specification,
//! `doc/specs/thrift-compact-protocol.md` in the Apache Thrift repository.

mod errors;
mod marks;
mod reader;
mod skip;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
mod types;
mod varint;

pub use errors::{ThriftError, VarintTarget};
pub use marks::Marks;
pub use reader::{CompactReader, FieldCursor};
#[cfg(feature = "internals")]
pub use types::ListHeader;
pub use types::{ElementType, Field, FieldType};

/// Maximum container nesting that [`CompactReader::skip`] descends into.
///
/// Parquet's own structs nest only a few levels, so this bounds the recursion an
/// attacker can cause through unknown fields without rejecting real files.
pub const SKIP_DEPTH_MAX: u16 = 64;

// `skip` must be able to descend at least one level, or every container is rejected.
const _: () = assert!(SKIP_DEPTH_MAX > 0);
