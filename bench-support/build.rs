//! Emits `PARKAY_PARQUET_LOCK_VERSIONS` from the workspace `Cargo.lock` so
//! `bench-support` can hash the locked `parquet` version into its corpus cache key
//! without embedding the entire `Cargo.lock` into benchmark and test binaries.

use std::path::Path;

fn main() {
    let lock_path = Path::new("../Cargo.lock");
    println!("cargo::rerun-if-changed={}", lock_path.display());
    let lockfile = std::fs::read_to_string(lock_path)
        .unwrap_or_else(|e| panic!("{}: {e}", lock_path.display()));
    let mut versions = Vec::new();
    let mut lines = lockfile.lines();
    while let Some(line) = lines.next() {
        if line.trim() == r#"name = "parquet""#
            && let Some(version_line) = lines.next()
        {
            versions.push(version_line.trim().to_owned());
        }
    }
    println!(
        "cargo::rustc-env=PARKAY_PARQUET_LOCK_VERSIONS={}",
        versions.join(",")
    );
}
