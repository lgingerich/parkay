//! Benchmark and profiling support for parkay: Parquet footer fixtures, the shared
//! compute-bound control workload, and a copy of arrow-rs's Thrift compact reader for
//! like-for-like comparison.

pub mod arrow_thrift;
pub mod corpus;
pub mod fixtures;

use std::hint::black_box;
use std::path::Path;

use fixtures::Fixture;
pub use fixtures::footer_tail;

/// Name of the compute-bound control benchmark registered in every group.
/// `xtask` uses this workload's drift to judge whether changes in the group are noise.
pub const CONTROL_WORKLOAD: &str = "scan";

/// Loads the benchmark corpus from `testdata_dir`, plus any `.parquet` files in
/// `$PARKAY_BENCH_DIR` (set by `cargo xtask bench --files DIR`).
///
/// # Panics
///
/// Panics if `$PARKAY_BENCH_DIR` points to an unreadable directory or contains an
/// invalid `.parquet` file.
#[must_use]
pub fn load_fixtures(testdata_dir: &Path) -> Vec<Fixture> {
    load_fixtures_inner(testdata_dir, true)
}

/// Loads only the footer fixtures of the benchmark corpus from `testdata_dir`, plus the
/// footers of any `.parquet` files in `$PARKAY_BENCH_DIR`, without reading page indexes.
///
/// # Panics
///
/// Panics if `$PARKAY_BENCH_DIR` points to an unreadable directory or contains an
/// invalid `.parquet` file.
#[must_use]
pub fn load_footer_fixtures(testdata_dir: &Path) -> Vec<Fixture> {
    load_fixtures_inner(testdata_dir, false)
}

fn load_fixtures_inner(testdata_dir: &Path, include_page_index: bool) -> Vec<Fixture> {
    let mut all = if include_page_index {
        corpus::all(testdata_dir)
    } else {
        corpus::footers(testdata_dir)
    };
    if let Some(dir) = std::env::var_os("PARKAY_BENCH_DIR") {
        let extra = if include_page_index {
            fixtures::from_dir(Path::new(&dir))
        } else {
            fixtures::footers_from_dir(Path::new(&dir))
        };
        all.extend(extra.unwrap_or_else(|e| panic!("{e}")));
    }
    all
}

/// One pass touching every byte (counting bytes with the high bit set): compute-bound
/// so it tracks CPU clock drift, unlike `memcpy` which is limited by memory bandwidth.
#[must_use]
fn scan_bytes(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte & 0x80 != 0).count()
}

/// Runs the [`CONTROL_WORKLOAD`] pass over every buffer in `footers`.
#[must_use]
pub fn run_control(footers: &[Vec<u8>]) -> usize {
    let mut total = 0;
    for footer in footers {
        total += black_box(scan_bytes(black_box(footer)));
    }
    total
}

#[cfg(test)]
pub(crate) struct TempDir(pub(crate) std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "parkay-bench-support-{label}-{}-{id}",
            std::process::id()
        ));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", path.display()));
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}
