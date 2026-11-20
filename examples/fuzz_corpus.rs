//! Writes seed corpora for the Thrift fuzz targets: real footers from parquet-testing
//! (up to 64 KiB each) and two small synthetic ones, each prefixed with the selector
//! byte for `Struct` (see `fuzz/fuzz_targets/common/mod.rs`). Seeds let the fuzzer
//! start from valid Thrift instead of random bytes.
//!
//! Run through `cargo xtask fuzz`, or directly with
//! `cargo run --release --example fuzz_corpus --features internals -- TARGET...`.

#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a one-shot tool that panics on bad fixtures"
)]

use std::fs;
use std::path::Path;

use bench_support::fixtures;

/// Seeds larger than this slow the fuzzer down more than they help.
const MAX_SEED: usize = 64 * 1024;

/// The selector byte that starts a target at `ElementType::Struct`.
const STRUCT_SELECTOR: u8 = 0;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut seeds = fixtures::parquet_testing(&root.join("testdata"));
    seeds.push(fixtures::synthetic(4, 3));
    seeds.push(fixtures::synthetic(3, 20));
    seeds.retain(|footer| footer.len() <= MAX_SEED);

    for target in std::env::args().skip(1) {
        let dir = root.join("fuzz/corpus").join(&target);
        fs::create_dir_all(&dir).unwrap();
        for (i, footer) in seeds.iter().enumerate() {
            let mut seed = vec![STRUCT_SELECTOR];
            seed.extend_from_slice(footer);
            fs::write(dir.join(format!("footer-{i:03}")), seed).unwrap();
        }
        println!("{target}: {} seeds in {}", seeds.len(), dir.display());
    }
}
