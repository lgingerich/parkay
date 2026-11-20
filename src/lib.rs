//! A high-performance Parquet reader and writer.
//!
//! See `docs/design.md` for the design and `ARCHITECTURE.md` for module boundaries.

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
// TODO(M2): remove once the footer parser calls this module from non-test code.
#[cfg_attr(not(test), expect(dead_code, reason = "no non-test caller until M2"))]
mod thrift;
