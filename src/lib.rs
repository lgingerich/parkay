//! A high-performance Parquet reader and writer.
//!
//! So far Parkay opens files and reads their metadata: see [`metadata`]. See
//! `docs/design.md` for the design and `ARCHITECTURE.md` for module boundaries.

#![warn(
    missing_debug_implementations,
    missing_docs,
    unnameable_types,
    unreachable_pub,
    clippy::cognitive_complexity,
    clippy::print_stderr,
    clippy::print_stdout
)]

mod limits;
pub mod metadata;

pub use limits::{FOOTER_SIZE_MAX_DEFAULT, Limits, SCHEMA_DEPTH_MAX_DEFAULT};

// Internal modules are public only under the `internals` feature, so the fuzz targets
// and benchmarks (separate crates) can reach them. Not a stable API.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod thrift;
#[cfg(not(feature = "internals"))]
#[expect(
    unreachable_pub,
    reason = "items are public only under the `internals` feature"
)]
mod thrift;
