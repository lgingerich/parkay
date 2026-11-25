//! Writes seed corpora for the fuzz targets: real footers from parquet-testing (up to
//! 64 KiB each), small synthetic footers, non-struct Thrift values for the Thrift
//! targets, and compact byte-stream seeds for structure-aware (`*_structured`) targets.
//!
//! Run through `cargo xtask fuzz`, or directly with
//! `cargo run --example fuzz_corpus -- TARGET...`.

#![expect(
    clippy::unwrap_used,
    reason = "a one-shot tool that panics on bad fixtures"
)]

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use bench_support::{corpus, fixtures};

/// Seeds larger than this slow the fuzzer down more than they help.
const MAX_SEED: usize = 64 * 1024;

/// Selector bytes matching `fuzz/fuzz_targets/common/mod.rs::TYPES`.
const STRUCT_SELECTOR: u8 = 0;

/// Small valid non-struct Thrift values `(selector, encoded_bytes)` so Thrift targets
/// start with coverage across lists, sets, maps, binaries, scalars, and UUIDs.
const THRIFT_EXTRA_SEEDS: &[(&str, u8, &[u8])] = &[
    // List of 3 i32s: header (size=3 << 4 | I32=5 = 0x35), varints 0, 2, 300.
    ("list-i32", 1, &[0x35, 0x00, 0x02, 0xAC, 0x02]),
    // List of 2 binaries: header (size=2 << 4 | Binary=8 = 0x28), "hi", "".
    ("list-bin", 1, &[0x28, 0x02, b'h', b'i', 0x00]),
    // Set of 2 i64s: header (size=2 << 4 | I64=6 = 0x26), varints 1, -1.
    ("set-i64", 2, &[0x26, 0x02, 0x01]),
    // Map of 1 binary -> i32: size=1, types=(Binary=8 << 4 | I32=5 = 0x85), "k" -> 42.
    ("map-bin-i32", 3, &[0x01, 0x85, 0x01, b'k', 0x54]),
    // Empty map: size=0.
    ("map-empty", 3, &[0x00]),
    // Binary of 4 bytes.
    ("binary", 4, &[0x04, b't', b'e', b's', b't']),
    // Bool element (true).
    ("bool", 5, &[0x01]),
    // I8, I16, I32, I64, Double, UUID.
    ("i8", 6, &[0x7F]),
    ("i16", 7, &[0xAC, 0x02]),
    ("i32", 8, &[0xFE, 0xFF, 0x03]),
    ("i64", 9, &[0x80, 0x80, 0x80, 0x80, 0x10]),
    ("double", 10, &[0, 0, 0, 0, 0, 0, 0xF0, 0x3F]),
    (
        "uuid",
        11,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
    ),
    // 6-level nested struct (crosses `INLINE_STACK = 4`).
    (
        "deep-struct",
        0,
        &[0x1C, 0x1C, 0x1C, 0x1C, 0x1C, 0x15, 0x02, 0, 0, 0, 0, 0, 0],
    ),
];

/// Builds a `List<Binary>` with 150 elements (selector 1 = `List`) to cross
/// `BINARIES_GROWTH_COUNT_MIN = 128` and exercise `grow_bins` + `skip_list_tail_growing`.
fn wide_binary_list_seed() -> Vec<u8> {
    let mut seed = Vec::with_capacity(4 + 150 * 2);
    seed.push(1); // selector: List
    seed.push(0xF8); // long size + Binary (8)
    seed.extend_from_slice(&[0x96, 0x01]); // varint(150)
    for i in 0_u8..150 {
        seed.push(0x01);
        seed.push(i);
    }
    seed
}

/// Deterministic byte seeds for structure-aware (`*_structured`) targets so every top-level
/// generator mode starts with clean coverage.
fn structured_seeds() -> Vec<Vec<u8>> {
    (0_u8..16)
        .map(|variant| {
            let mut bytes = Vec::with_capacity(64);
            let mut state = u64::from(variant).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A;
            bytes.push(variant);
            for _ in 0..63 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                bytes.push((state >> 56) as u8);
            }
            bytes
        })
        .collect()
}

fn valid_target_name(root: &Path, target: &str) -> bool {
    !target.is_empty()
        && !target.contains(['/', '\\'])
        && target != "."
        && target != ".."
        && root
            .join(format!("fuzz/fuzz_targets/{target}.rs"))
            .is_file()
}

fn main() -> ExitCode {
    let targets: Vec<String> = std::env::args().skip(1).collect();
    if targets.is_empty() {
        eprintln!("usage: fuzz_corpus TARGET...");
        return ExitCode::FAILURE;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for target in &targets {
        if !valid_target_name(root, target) {
            eprintln!("fuzz_corpus: unknown fuzz target `{target}`");
            return ExitCode::FAILURE;
        }
    }

    let mut seeds = fixtures::parquet_testing(&root.join("testdata"));
    // Row groups whose chunks differ only in offsets, row groups of random data, and
    // row groups whose column chunks change shape, for the template-comparison pass.
    for file in [
        corpus::uniform_file(4, 3),
        corpus::uniform_file(3, 20),
        corpus::varied_file(6, 12),
        corpus::alternating_file(4, 6),
    ] {
        seeds.push(fixtures::footer(&file).unwrap().to_vec());
    }
    seeds.retain(|footer| footer.len() <= MAX_SEED);

    for target in &targets {
        let dir = root.join("fuzz/corpus").join(target);
        fs::create_dir_all(&dir).unwrap();
        if target.ends_with("_structured") {
            let synth = structured_seeds();
            for (i, bytes) in synth.iter().enumerate() {
                fs::write(dir.join(format!("synth-{i:02}")), bytes).unwrap();
            }
            println!("{target}: {} seeds in {}", synth.len(), dir.display());
            continue;
        }
        for (i, footer) in seeds.iter().enumerate() {
            let mut seed = vec![STRUCT_SELECTOR];
            seed.extend_from_slice(footer);
            fs::write(dir.join(format!("footer-{i:03}")), seed).unwrap();
        }
        let mut count = seeds.len();
        if target.starts_with("thrift_") {
            for (name, selector, bytes) in THRIFT_EXTRA_SEEDS {
                let mut seed = vec![*selector];
                seed.extend_from_slice(bytes);
                fs::write(dir.join(format!("thrift-{name}")), seed).unwrap();
                count += 1;
            }
            fs::write(dir.join("thrift-wide-bin-list"), wide_binary_list_seed()).unwrap();
            count += 1;
        }
        println!("{target}: {count} seeds in {}", dir.display());
    }
    ExitCode::SUCCESS
}
