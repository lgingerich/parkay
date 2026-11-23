//! The benchmark corpus: a few Parquet files chosen to cover the shapes real files
//! take, fed through every benchmark the same way.
//!
//! | Entry | Shape | Stands for |
//! |---|---|---|
//! | `small` | 16 columns, 1 row group | The most common file; opening it is mostly fixed cost. |
//! | `medium` | 100 columns, 50 row groups, sparse columns, bloom filters, a large key-value blob | A typical analytics file. |
//! | `many_row_groups` | 20 columns, 2,000 row groups | Many small column chunks. |
//! | `wide` | 1,000 columns, 20 row groups | A footer beyond the CPU caches. |
//! | `very_wide` | 50,000 columns, 2 row groups | Feature tables. |
//! | `nested` | structs, lists, maps, and lists of structs of lists | Schema decoding and the leaf mapping. |
//! | `legacy` | six files from parquet-testing | Layouts arrow-rs no longer writes (Impala, Spark, old lists). |
//! | `many_files` | 128 distinct files of one table | Planning over a table: no cache or predictor warmth from one footer to the next. |
//!
//! Synthetic files hold random values (`SplitMix64`, fixed seeds), strings of 3 to 64
//! letters, low-cardinality strings, nulls at rates that change between row groups, and
//! columns that are all null in some row groups. They are written by arrow-rs with
//! statistics truncated to 64 bytes and a page index, as most current writers do. Data
//! pages are uncompressed: no benchmark reads them yet.
//!
//! Debug builds use the same entries at a fraction of their size (see `SMOKE`).
//!
//! Generating the corpus takes a while, so only the extracted artifacts for each entry
//! (raw footers, optional page indexes, and original file sizes) are cached once under
//! `target/bench-corpus/<mode>-<KEY>/` (or `$PARKAY_CORPUS_DIR/<mode>-<KEY>/`), where
//! KEY hashes the cache format version, this file's source, the locked `parquet` crate
//! version, and `SMOKE`. Full synthetic `.parquet` bytes are discarded immediately after
//! extraction, cache hits read the extracted bytes directly without invoking arrow-rs,
//! and stale cache directories of the same mode (plus legacy unprefixed directories) are
//! pruned whenever a fresh cache directory is created.

use std::fmt::Write as _;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parquet::basic::Compression;
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int32Type, Int64Type};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::{ColumnPath, Type};

use crate::fixtures::{Contents, Fixture, footer, page_index_range};

/// Cache format version tag included in the cache key hash. Bump whenever the on-disk
/// artifact layout under `target/bench-corpus/` changes.
const CACHE_FORMAT_VERSION: &str = "v2-extracted-artifacts";

/// The files of parquet-testing in the `legacy` entry: Impala's oldest layout, Spark
/// with its schema in key-value metadata, two-level and unannotated lists, and
/// parquet-mr's nested maps and lists.
const LEGACY: [&str; 6] = [
    "alltypes_plain.parquet",
    "int96_from_spark.parquet",
    "old_list_structure.parquet",
    "repeated_no_annotation.parquet",
    "nested_maps.snappy.parquet",
    "nested_lists.snappy.parquet",
];

/// An unoptimized build is a smoke test, not a measurement: `cargo xtask check` runs
/// every benchmark once as a test, in a debug build, one process each. There the corpus
/// keeps every entry's shape at a fraction of its size, so each process loads it in
/// milliseconds instead of seconds, and a fresh checkout does not generate the full
/// corpus over a hundred times at once. Benchmarks build optimized and use full sizes.
const SMOKE: bool = cfg!(debug_assertions);

/// `full` in a benchmark build, `smoke` in a smoke-test build.
const fn size(full: u64, smoke: u64) -> u64 {
    if SMOKE { smoke } else { full }
}

/// Every corpus entry's footers, then the page indexes of the entries that have one.
/// `testdata_dir` is parkay's `testdata` directory.
#[must_use]
pub fn all(testdata_dir: &Path) -> Vec<Fixture> {
    load_corpus(testdata_dir, true)
}

/// Every corpus entry's footers only, without reading cached `*.page_index` artifacts.
/// `testdata_dir` is parkay's `testdata` directory.
#[must_use]
pub fn footers(testdata_dir: &Path) -> Vec<Fixture> {
    load_corpus(testdata_dir, false)
}

/// Extracted artifacts for one corpus entry.
struct CachedEntry {
    footers: Vec<Vec<u8>>,
    file_sizes: Vec<u64>,
    page_indexes: Vec<Vec<u8>>,
    page_index_file_sizes: Vec<u64>,
}

/// RAII guard for a staging directory during cache generation so panics or failed
/// writes never leave orphan `.partial-*` directories behind.
struct StagingDir {
    path: PathBuf,
    committed: bool,
}

impl StagingDir {
    fn create(path: PathBuf) -> Self {
        drop(fs::remove_dir_all(&path));
        fs::create_dir(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Self {
            path,
            committed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(mut self, target: &Path) {
        match fs::rename(&self.path, target) {
            Ok(()) => self.committed = true,
            Err(err) => {
                // Another process may have finished first and populated `target`.
                assert!(
                    target.is_dir(),
                    "failed to rename {} to {}: {err}",
                    self.path.display(),
                    target.display()
                );
            }
        }
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.committed {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}

fn load_corpus(testdata_dir: &Path, include_page_index: bool) -> Vec<Fixture> {
    let mut footer_fixtures = Vec::new();
    let mut page_index_fixtures = Vec::new();
    for (name, entry) in entries(testdata_dir, include_page_index) {
        if include_page_index && !entry.page_indexes.is_empty() {
            page_index_fixtures.push(Fixture {
                name: format!("{name}_page_index"),
                contents: Contents::PageIndex,
                file_sizes: entry.page_index_file_sizes,
                footers: entry.page_indexes,
            });
        }
        footer_fixtures.push(Fixture {
            name: name.to_owned(),
            contents: Contents::Footer,
            footers: entry.footers,
            file_sizes: entry.file_sizes,
        });
    }
    footer_fixtures.extend(page_index_fixtures);
    footer_fixtures
}

/// Every entry's name and extracted artifacts.
fn entries(testdata_dir: &Path, include_page_index: bool) -> Vec<(&'static str, CachedEntry)> {
    let cache = cache_dir();
    let one = |name: &'static str, write: &dyn Fn() -> Vec<u8>| {
        (
            name,
            load_cached(&cache, name, 1, include_page_index, &|_| write()),
        )
    };
    vec![
        one("small", &|| flat(&Flat::new(16, 1, size(5_000, 100), 1))),
        one("medium", &|| {
            flat(&Flat {
                bloom_filters: true,
                key_value_blob: true,
                ..Flat::new(size(100, 24), size(50, 4), size(1_000, 50), 2)
            })
        }),
        one("many_row_groups", &|| {
            flat(&Flat::new(20, size(2_000, 40), size(200, 10), 3))
        }),
        one("wide", &|| {
            flat(&Flat::new(size(1_000, 120), size(20, 2), size(200, 10), 4))
        }),
        one("very_wide", &|| {
            flat(&Flat::new(size(50_000, 600), 2, size(20, 5), 5))
        }),
        one("nested", &|| nested(size(20, 3), size(1_000, 50), 6)),
        ("legacy", legacy(testdata_dir, include_page_index)),
        (
            "many_files",
            load_cached(
                &cache,
                "many_files",
                size(128, 4),
                include_page_index,
                &|index| {
                    flat(&Flat::new(
                        size(60, 16),
                        size(6, 2),
                        size(200, 10),
                        100 + index,
                    ))
                },
            ),
        ),
    ]
}

/// `target/bench-corpus/<mode>-<KEY>`, or `$PARKAY_CORPUS_DIR/<mode>-<KEY>`.
/// Creates the directory and prunes stale sibling caches on a fresh key.
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("PARKAY_CORPUS_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/bench-corpus"),
        PathBuf::from,
    );
    let mut hasher = DefaultHasher::new();
    CACHE_FORMAT_VERSION.hash(&mut hasher);
    include_str!("corpus.rs").hash(&mut hasher);
    include_str!("fixtures.rs").hash(&mut hasher);
    env!("PARKAY_PARQUET_LOCK_VERSIONS").hash(&mut hasher);
    SMOKE.hash(&mut hasher);
    let mode_prefix = if SMOKE { "smoke-" } else { "full-" };
    let current_name = format!("{mode_prefix}{:016x}", hasher.finish());
    prepare_cache_dir(&base, &current_name, mode_prefix)
}

/// Ensures `base/current_name` exists; when creating it for the first time, prunes stale
/// sibling cache directories of the same mode and any legacy unprefixed 16-hex caches.
fn prepare_cache_dir(base: &Path, current_name: &str, mode_prefix: &str) -> PathBuf {
    let cache = base.join(current_name);
    if !cache.is_dir() {
        fs::create_dir_all(&cache).unwrap_or_else(|e| panic!("{}: {e}", cache.display()));
        prune_stale_caches(base, current_name, mode_prefix);
    }
    cache
}

/// Removes stale sibling cache directories under `base` that match `mode_prefix` or the
/// legacy unprefixed 16-hex-character key format, leaving `current_name` and the other
/// build mode (`smoke-` vs `full-`) intact.
fn prune_stale_caches(base: &Path, current_name: &str, mode_prefix: &str) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name == current_name {
            continue;
        }
        if name.starts_with(mode_prefix) || is_legacy_cache_dir(name) {
            drop(fs::remove_dir_all(entry.path()));
        }
    }
}

fn is_legacy_cache_dir(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn slice_page_index<'a>(file: &'a [u8], start: u64, end: u64, context: &str) -> &'a [u8] {
    let start =
        usize::try_from(start).unwrap_or_else(|_| panic!("{context}: page index start overflow"));
    let end = usize::try_from(end).unwrap_or_else(|_| panic!("{context}: page index end overflow"));
    file.get(start..end).unwrap_or_else(|| {
        panic!(
            "{context}: page index range {start}..{end} out of bounds for {}-byte file",
            file.len()
        )
    })
}

/// Loads the extracted artifacts for `count` files of entry `name` from `cache`,
/// generating and caching them on a miss.
///
/// On a miss, each `.parquet` file is generated in memory, its `(footer, optional
/// page_index, file_size)` are written to a staging directory, and the full Parquet
/// bytes are dropped immediately. The staging directory is renamed into place so an
/// interrupted run leaves no partial entry behind. On a hit, cached artifacts are read
/// directly without decoding metadata.
fn load_cached(
    cache: &Path,
    name: &str,
    count: u64,
    include_page_index: bool,
    write: &dyn Fn(u64) -> Vec<u8>,
) -> CachedEntry {
    let dir = cache.join(name);
    if !dir.is_dir() {
        static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging = StagingDir::create(
            cache.join(format!("{name}.partial-{}-{nonce}", std::process::id())),
        );
        let mut sizes_text = String::new();
        for index in 0..count {
            let file = write(index);
            let file_size = file.len() as u64;
            let raw_footer = footer(&file)
                .unwrap_or_else(|| panic!("{name}/{index:04}: generated invalid Parquet file"));
            let footer_path = staging.path().join(format!("{index:04}.footer"));
            fs::write(&footer_path, raw_footer)
                .unwrap_or_else(|e| panic!("{}: {e}", footer_path.display()));
            if let Some((start, end)) = page_index_range(raw_footer) {
                let page_index = slice_page_index(&file, start, end, &format!("{name}/{index:04}"));
                let page_index_path = staging.path().join(format!("{index:04}.page_index"));
                fs::write(&page_index_path, page_index)
                    .unwrap_or_else(|e| panic!("{}: {e}", page_index_path.display()));
            }
            let _ = writeln!(sizes_text, "{file_size}");
            drop(file);
        }
        let sizes_path = staging.path().join("sizes.txt");
        fs::write(&sizes_path, sizes_text)
            .unwrap_or_else(|e| panic!("{}: {e}", sizes_path.display()));
        staging.commit(&dir);
    }

    let sizes_path = dir.join("sizes.txt");
    let sizes_text =
        fs::read_to_string(&sizes_path).unwrap_or_else(|e| panic!("{}: {e}", sizes_path.display()));
    let file_sizes: Vec<u64> = sizes_text
        .lines()
        .map(|line| {
            line.parse::<u64>()
                .unwrap_or_else(|e| panic!("{}: invalid size {line:?}: {e}", sizes_path.display()))
        })
        .collect();
    let expected = usize::try_from(count).unwrap_or_else(|_| panic!("{name}: count overflow"));
    assert_eq!(
        file_sizes.len(),
        expected,
        "{}: expected {expected} sizes, got {}",
        sizes_path.display(),
        file_sizes.len()
    );

    let mut footers = Vec::with_capacity(expected);
    let mut page_indexes = Vec::new();
    let mut page_index_file_sizes = Vec::new();
    for (index, &file_size) in file_sizes.iter().enumerate() {
        let footer_path = dir.join(format!("{index:04}.footer"));
        footers.push(
            fs::read(&footer_path).unwrap_or_else(|e| panic!("{}: {e}", footer_path.display())),
        );
        if include_page_index {
            let page_index_path = dir.join(format!("{index:04}.page_index"));
            match fs::read(&page_index_path) {
                Ok(bytes) => {
                    page_indexes.push(bytes);
                    page_index_file_sizes.push(file_size);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => panic!("{}: {e}", page_index_path.display()),
            }
        }
    }

    CachedEntry {
        footers,
        file_sizes,
        page_indexes,
        page_index_file_sizes,
    }
}

fn legacy(testdata_dir: &Path, include_page_index: bool) -> CachedEntry {
    let dir = testdata_dir.join("parquet-testing/data");
    let mut footers = Vec::with_capacity(LEGACY.len());
    let mut file_sizes = Vec::with_capacity(LEGACY.len());
    let mut page_indexes = Vec::new();
    let mut page_index_file_sizes = Vec::new();
    for name in LEGACY {
        let file = fs::read(dir.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e} (run `git submodule update --init`)"));
        let file_size = file.len() as u64;
        let raw_footer =
            footer(&file).unwrap_or_else(|| panic!("{name}: not an unencrypted Parquet file"));
        if include_page_index && let Some((start, end)) = page_index_range(raw_footer) {
            page_indexes.push(slice_page_index(&file, start, end, name).to_vec());
            page_index_file_sizes.push(file_size);
        }
        footers.push(raw_footer.to_vec());
        file_sizes.push(file_size);
    }
    CachedEntry {
        footers,
        file_sizes,
        page_indexes,
        page_index_file_sizes,
    }
}

/// A small deterministic random number generator (`SplitMix64`).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// True about once in `one_in` calls.
    fn one_in(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    /// A lowercase word of 3 to 64 letters.
    fn word(&mut self) -> ByteArray {
        let len = 3 + self.below(62);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "below(26) is always in 0..26, fitting in u8"
        )]
        let letters: Vec<u8> = (0..len).map(|_| b'a' + self.below(26) as u8).collect();
        ByteArray::from(letters)
    }
}

/// The kinds of column in a flat file, repeating in this order.
const FLAT_KINDS: [&str; 8] = [
    "required int64",
    "optional int32",
    "required double",
    "optional binary",
    "required binary",
    "required boolean",
    "optional int64",
    "optional int64",
];

/// A flat file's shape.
struct Flat {
    columns: u64,
    row_groups: u64,
    rows: u64,
    seed: u64,
    /// A bloom filter on every low-cardinality string column.
    bloom_filters: bool,
    /// A few kilobytes of key-value metadata, as Spark and pandas write their schemas.
    key_value_blob: bool,
    /// Column kind 7 is all null in odd row groups, so its chunks alternate in shape.
    sparse_alternating: bool,
}

impl Flat {
    fn new(columns: u64, row_groups: u64, rows: u64, seed: u64) -> Self {
        Self {
            columns,
            row_groups,
            rows,
            seed,
            bloom_filters: false,
            key_value_blob: false,
            sparse_alternating: true,
        }
    }
}

/// A flat file of `FLAT_KINDS` columns. Column kind 6 is a timestamp; when
/// `sparse_alternating` is set, kind 7 is all null in odd row groups.
#[expect(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "synthetic in-memory Parquet generator with bounded corpus dimensions"
)]
fn flat(shape: &Flat) -> Vec<u8> {
    let mut fields = String::new();
    for c in 0..shape.columns {
        let kind = FLAT_KINDS[(c % 8) as usize];
        let annotation = match c % 8 {
            3 | 4 => " (STRING)",
            6 => " (TIMESTAMP(MILLIS,true))",
            _ => "",
        };
        let _ = write!(fields, "{kind} column_{c}{annotation};");
    }
    let schema = Arc::new(parse_message_type(&format!("message m {{ {fields} }}")).unwrap());
    let mut properties = properties();
    if shape.bloom_filters {
        for c in (4..shape.columns).step_by(8) {
            properties = properties
                .set_column_bloom_filter_enabled(ColumnPath::from(format!("column_{c}")), true);
        }
    }
    if shape.key_value_blob {
        properties = properties.set_key_value_metadata(Some(vec![KeyValue::new(
            "org.apache.spark.sql.parquet.row.metadata".to_owned(),
            spark_schema(shape.columns),
        )]));
    }
    let mut random = Random(shape.seed);
    let few_words: Vec<ByteArray> = (0..12).map(|_| random.word()).collect();
    let mut file = Vec::new();
    let mut writer =
        SerializedFileWriter::new(&mut file, schema, Arc::new(properties.build())).unwrap();
    for row_group_index in 0..shape.row_groups {
        let rows = shape.rows as usize;
        let null_one_in = 2 + random.below(30);
        let mut row_group = writer.next_row_group().unwrap();
        let mut c = 0_u64;
        while let Some(mut column) = row_group.next_column().unwrap() {
            let levels = |rng: &mut Random, all_null: bool| -> (Vec<i16>, usize) {
                let levels: Vec<i16> = (0..rows)
                    .map(|_| i16::from(!all_null && !rng.one_in(null_one_in)))
                    .collect();
                let present = levels.iter().filter(|&&l| l == 1).count();
                (levels, present)
            };
            match c % 8 {
                0 => {
                    let start = (row_group_index * shape.rows) as i64;
                    let values: Vec<i64> = (0..rows as i64).map(|i| start + i).collect();
                    column
                        .typed::<Int64Type>()
                        .write_batch(&values, None, None)
                        .unwrap();
                }
                1 => {
                    let (levels, present) = levels(&mut random, false);
                    let values: Vec<i32> = (0..present)
                        .map(|_| random.below(1_000_000) as i32)
                        .collect();
                    column
                        .typed::<Int32Type>()
                        .write_batch(&values, Some(&levels), None)
                        .unwrap();
                }
                2 => {
                    let values: Vec<f64> = (0..rows)
                        .map(|_| random.below(1 << 40) as f64 / 1e4)
                        .collect();
                    column
                        .typed::<DoubleType>()
                        .write_batch(&values, None, None)
                        .unwrap();
                }
                3 => {
                    let (levels, present) = levels(&mut random, false);
                    let values: Vec<ByteArray> = (0..present).map(|_| random.word()).collect();
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&values, Some(&levels), None)
                        .unwrap();
                }
                4 => {
                    let values: Vec<ByteArray> = (0..rows)
                        .map(|_| few_words[random.below(12) as usize].clone())
                        .collect();
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&values, None, None)
                        .unwrap();
                }
                5 => {
                    let values: Vec<bool> = (0..rows).map(|_| random.one_in(3)).collect();
                    column
                        .typed::<BoolType>()
                        .write_batch(&values, None, None)
                        .unwrap();
                }
                6 => {
                    let (levels, present) = levels(&mut random, false);
                    let base = 1_700_000_000_000_i64 + (row_group_index as i64) * 3_600_000;
                    let values: Vec<i64> = (0..present)
                        .map(|_| base + random.below(3_600_000) as i64)
                        .collect();
                    column
                        .typed::<Int64Type>()
                        .write_batch(&values, Some(&levels), None)
                        .unwrap();
                }
                _ => {
                    let all_null = shape.sparse_alternating && row_group_index % 2 == 1;
                    let (levels, present) = levels(&mut random, all_null);
                    let values: Vec<i64> = (0..present).map(|_| random.next() as i64).collect();
                    column
                        .typed::<Int64Type>()
                        .write_batch(&values, Some(&levels), None)
                        .unwrap();
                }
            }
            column.close().unwrap();
            c += 1;
        }
        row_group.close().unwrap();
    }
    writer.close().unwrap();
    file
}

/// A file whose row groups write identical values, so a column's chunks differ between
/// row groups only in their file offsets. Used by unit tests and fuzz seeding.
///
/// # Panics
///
/// Panics if arrow-rs fails to encode the in-memory Parquet file.
#[must_use]
#[expect(
    clippy::unwrap_used,
    reason = "synthetic in-memory Parquet generator with static schema"
)]
pub fn uniform_file(columns: usize, row_groups: usize) -> Vec<u8> {
    let mut fields = String::new();
    for c in 0..columns {
        if c % 2 == 0 {
            let _ = write!(fields, "required int64 c{c};");
        } else {
            let _ = write!(fields, "required binary c{c} (STRING);");
        }
    }
    let schema = Arc::new(parse_message_type(&format!("message m {{ {fields} }}")).unwrap());
    let props = Arc::new(properties().build());
    let ints: Vec<i64> = (0..4).map(|i| i * 1_000_003).collect();
    let strings: Vec<ByteArray> = ["alpha", "bravo", "charlie", "delta"]
        .into_iter()
        .map(ByteArray::from)
        .collect();

    let mut file = Vec::new();
    let mut writer = SerializedFileWriter::new(&mut file, schema, props).unwrap();
    for _ in 0..row_groups {
        let mut row_group = writer.next_row_group().unwrap();
        let mut c = 0;
        while let Some(mut column) = row_group.next_column().unwrap() {
            if c % 2 == 0 {
                column
                    .typed::<Int64Type>()
                    .write_batch(&ints, None, None)
                    .unwrap();
            } else {
                column
                    .typed::<ByteArrayType>()
                    .write_batch(&strings, None, None)
                    .unwrap();
            }
            column.close().unwrap();
            c += 1;
        }
        row_group.close().unwrap();
    }
    writer.close().unwrap();
    file
}

/// A file of mixed types holding random values, strings of random lengths, and random
/// nulls, without all-null alternating columns so every chunk keeps the same field set.
/// Used by unit tests and fuzz seeding.
#[must_use]
pub fn varied_file(columns: usize, row_groups: usize) -> Vec<u8> {
    flat(&Flat {
        sparse_alternating: false,
        ..Flat::new(columns as u64, row_groups as u64, 40, 0x5EED)
    })
}

/// A file of optional `int64` columns whose chunks hold values in even row groups and
/// only nulls in odd row groups, so every column changes field set every row group.
/// Used by unit tests and fuzz seeding.
///
/// # Panics
///
/// Panics if arrow-rs fails to encode the in-memory Parquet file.
#[must_use]
#[expect(
    clippy::unwrap_used,
    reason = "synthetic in-memory Parquet generator with static schema"
)]
pub fn alternating_file(columns: usize, row_groups: usize) -> Vec<u8> {
    let mut fields = String::new();
    for c in 0..columns {
        let _ = write!(fields, "optional int64 c{c};");
    }
    let schema = Arc::new(parse_message_type(&format!("message m {{ {fields} }}")).unwrap());
    let props = Arc::new(properties().build());
    let values: Vec<i64> = (0..4).map(|i| i * 1_000_003).collect();

    let mut file = Vec::new();
    let mut writer = SerializedFileWriter::new(&mut file, schema, props).unwrap();
    for index in 0..row_groups {
        let mut row_group = writer.next_row_group().unwrap();
        while let Some(mut column) = row_group.next_column().unwrap() {
            let typed = column.typed::<Int64Type>();
            if index % 2 == 0 {
                typed.write_batch(&values, Some(&[1; 4]), None).unwrap();
            } else {
                typed.write_batch(&[], Some(&[0; 4]), None).unwrap();
            }
            column.close().unwrap();
        }
        row_group.close().unwrap();
    }
    writer.close().unwrap();
    file
}

/// Writer settings shared by every synthetic file, set explicitly rather than taken
/// from arrow-rs's defaults, which change between releases.
fn properties() -> parquet::file::properties::WriterPropertiesBuilder {
    WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_statistics_truncate_length(Some(64))
        .set_dictionary_enabled(true)
}

/// A schema description of the size Spark writes for `columns` columns.
fn spark_schema(columns: u64) -> String {
    let fields: Vec<String> = (0..columns)
        .map(|c| {
            format!(r#"{{"name":"column_{c}","type":"long","nullable":true,"metadata":{{}}}}"#)
        })
        .collect();
    format!(r#"{{"type":"struct","fields":[{}]}}"#, fields.join(","))
}

/// One row of the `nested` file.
struct NestedRow {
    id: i64,
    address: Option<Address>,
    tags: Option<Vec<Option<ByteArray>>>,
    attributes: Option<Vec<(ByteArray, Option<i64>)>>,
    events: Option<Vec<Option<Event>>>,
}

struct Address {
    city: ByteArray,
    zip: Option<ByteArray>,
    geo: Option<(f64, f64)>,
}

struct Event {
    timestamp: i64,
    kind: Option<ByteArray>,
    payload: Option<Vec<Option<i32>>>,
}

const NESTED_SCHEMA: &str = "
message nested {
  required int64 id;
  optional group address {
    required binary city (STRING);
    optional binary zip (STRING);
    optional group geo { required double lat; required double lon; }
  }
  optional group tags (LIST) { repeated group list { optional binary element (STRING); } }
  optional group attributes (MAP) {
    repeated group key_value { required binary key (STRING); optional int64 value; }
  }
  optional group events (LIST) {
    repeated group list {
      optional group element {
        required int64 ts;
        optional binary kind (STRING);
        optional group payload (LIST) { repeated group list { optional int32 element; } }
      }
    }
  }
}";

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "synthetic nested test data with bounded random values"
)]
fn nested_row(random: &mut Random, id: i64) -> NestedRow {
    let maybe = |rng: &mut Random| !rng.one_in(6);
    let length = |rng: &mut Random| rng.below(5) as usize;
    NestedRow {
        id,
        address: maybe(random).then(|| Address {
            city: random.word(),
            zip: maybe(random).then(|| random.word()),
            geo: maybe(random).then(|| (random.below(180) as f64, random.below(360) as f64)),
        }),
        tags: maybe(random).then(|| {
            (0..length(random))
                .map(|_| maybe(random).then(|| random.word()))
                .collect()
        }),
        attributes: maybe(random).then(|| {
            (0..length(random))
                .map(|_| (random.word(), maybe(random).then(|| random.next() as i64)))
                .collect()
        }),
        events: maybe(random).then(|| {
            (0..length(random))
                .map(|_| {
                    maybe(random).then(|| Event {
                        timestamp: random.next() as i64,
                        kind: maybe(random).then(|| random.word()),
                        payload: maybe(random).then(|| {
                            (0..length(random))
                                .map(|_| maybe(random).then(|| random.below(1000) as i32))
                                .collect()
                        }),
                    })
                })
                .collect()
        }),
    }
}

/// One leaf column's values and its definition and repetition levels.
struct Leaf<T> {
    values: Vec<T>,
    definition: Vec<i16>,
    repetition: Vec<i16>,
}

impl<T> Leaf<T> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            definition: Vec::new(),
            repetition: Vec::new(),
        }
    }

    fn push(&mut self, definition: i16, repetition: i16, value: Option<T>) {
        self.definition.push(definition);
        self.repetition.push(repetition);
        self.values.extend(value);
    }
}

/// The levels of a leaf under one optional list whose items are optional (or are
/// optional groups holding a required leaf): a missing list, an empty list, then each
/// item, at definition levels `base` to `base + 3`.
fn list_leaf<I, T>(
    leaf: &mut Leaf<T>,
    list: Option<&[I]>,
    base: i16,
    value: impl Fn(&I) -> Option<T>,
) {
    match list {
        None => leaf.push(base, 0, None),
        Some([]) => leaf.push(base + 1, 0, None),
        Some(items) => {
            for (index, item) in items.iter().enumerate() {
                let repetition = i16::from(index > 0);
                match value(item) {
                    Some(v) => leaf.push(base + 3, repetition, Some(v)),
                    None => leaf.push(base + 2, repetition, None),
                }
            }
        }
    }
}

/// A file with structs, a list, a map, and a list of structs holding a list, in
/// `row_groups` row groups of `rows` rows.
#[expect(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    reason = "synthetic in-memory Parquet generator with static schema"
)]
fn nested(row_groups: u64, rows: u64, seed: u64) -> Vec<u8> {
    let schema: Arc<Type> = Arc::new(parse_message_type(NESTED_SCHEMA).unwrap());
    let mut random = Random(seed);
    let mut file = Vec::new();
    let mut writer =
        SerializedFileWriter::new(&mut file, schema, Arc::new(properties().build())).unwrap();
    for row_group_index in 0..row_groups {
        let rows: Vec<NestedRow> = (0..rows)
            .map(|row| nested_row(&mut random, (row_group_index * 1_000_000 + row) as i64))
            .collect();
        let mut row_group = writer.next_row_group().unwrap();
        let mut leaf_index = 0;
        while let Some(mut column) = row_group.next_column().unwrap() {
            write_nested_leaf(&mut column, leaf_index, &rows);
            column.close().unwrap();
            leaf_index += 1;
        }
        row_group.close().unwrap();
    }
    writer.close().unwrap();
    file
}

/// Writes leaf `index` of `NESTED_SCHEMA`, in schema order.
#[expect(
    clippy::unwrap_used,
    clippy::too_many_lines,
    reason = "synthetic in-memory Parquet generator writing each leaf of NESTED_SCHEMA"
)]
fn write_nested_leaf(
    column: &mut parquet::file::writer::SerializedColumnWriter<'_>,
    index: usize,
    rows: &[NestedRow],
) {
    fn write<T: parquet::data_type::DataType>(
        column: &mut parquet::file::writer::SerializedColumnWriter<'_>,
        leaf: &Leaf<T::T>,
        repeated: bool,
    ) {
        let repetition = repeated.then_some(leaf.repetition.as_slice());
        column
            .typed::<T>()
            .write_batch(&leaf.values, Some(&leaf.definition), repetition)
            .unwrap();
    }
    match index {
        0 => {
            let values: Vec<i64> = rows.iter().map(|r| r.id).collect();
            column
                .typed::<Int64Type>()
                .write_batch(&values, None, None)
                .unwrap();
        }
        1 | 2 => {
            let mut leaf = Leaf::new();
            for row in rows {
                match &row.address {
                    None => leaf.push(0, 0, None),
                    Some(address) if index == 1 => leaf.push(1, 0, Some(address.city.clone())),
                    Some(address) => match &address.zip {
                        Some(zip) => leaf.push(2, 0, Some(zip.clone())),
                        None => leaf.push(1, 0, None),
                    },
                }
            }
            write::<ByteArrayType>(column, &leaf, false);
        }
        3 | 4 => {
            let mut leaf = Leaf::new();
            for row in rows {
                match &row.address {
                    None => leaf.push(0, 0, None),
                    Some(Address { geo: None, .. }) => leaf.push(1, 0, None),
                    Some(Address {
                        geo: Some((lat, lon)),
                        ..
                    }) => {
                        leaf.push(2, 0, Some(if index == 3 { *lat } else { *lon }));
                    }
                }
            }
            write::<DoubleType>(column, &leaf, false);
        }
        5 => {
            let mut leaf = Leaf::new();
            for row in rows {
                list_leaf(&mut leaf, row.tags.as_deref(), 0, Clone::clone);
            }
            write::<ByteArrayType>(column, &leaf, true);
        }
        6 => {
            // The key is required: a present entry is definition level 2.
            let mut leaf = Leaf::new();
            for row in rows {
                match row.attributes.as_deref() {
                    None => leaf.push(0, 0, None),
                    Some([]) => leaf.push(1, 0, None),
                    Some(entries) => {
                        for (i, (key, _)) in entries.iter().enumerate() {
                            leaf.push(2, i16::from(i > 0), Some(key.clone()));
                        }
                    }
                }
            }
            write::<ByteArrayType>(column, &leaf, true);
        }
        7 => {
            let mut leaf = Leaf::new();
            for row in rows {
                list_leaf(&mut leaf, row.attributes.as_deref(), 0, |(_, value)| *value);
            }
            write::<Int64Type>(column, &leaf, true);
        }
        8 => {
            // `ts` is required inside an optional element: present is level 3.
            let mut leaf = Leaf::new();
            for row in rows {
                list_leaf(&mut leaf, row.events.as_deref(), 0, |event| {
                    event.as_ref().map(|e| e.timestamp)
                });
            }
            write::<Int64Type>(column, &leaf, true);
        }
        9 => {
            let mut leaf = Leaf::new();
            for row in rows {
                match row.events.as_deref() {
                    None => leaf.push(0, 0, None),
                    Some([]) => leaf.push(1, 0, None),
                    Some(events) => {
                        for (i, event) in events.iter().enumerate() {
                            let repetition = i16::from(i > 0);
                            match event {
                                None => leaf.push(2, repetition, None),
                                Some(Event { kind: None, .. }) => leaf.push(3, repetition, None),
                                Some(Event {
                                    kind: Some(kind), ..
                                }) => {
                                    leaf.push(4, repetition, Some(kind.clone()));
                                }
                            }
                        }
                    }
                }
            }
            write::<ByteArrayType>(column, &leaf, true);
        }
        _ => {
            // events.list.element.payload.list.element: two levels of repetition.
            let mut leaf = Leaf::new();
            for row in rows {
                match row.events.as_deref() {
                    None => leaf.push(0, 0, None),
                    Some([]) => leaf.push(1, 0, None),
                    Some(events) => {
                        for (i, event) in events.iter().enumerate() {
                            let event_repetition = i16::from(i > 0);
                            match event {
                                None => leaf.push(2, event_repetition, None),
                                Some(Event { payload: None, .. }) => {
                                    leaf.push(3, event_repetition, None);
                                }
                                Some(Event {
                                    payload: Some(items),
                                    ..
                                }) if items.is_empty() => {
                                    leaf.push(4, event_repetition, None);
                                }
                                Some(Event {
                                    payload: Some(items),
                                    ..
                                }) => {
                                    for (j, item) in items.iter().enumerate() {
                                        let repetition = if j == 0 { event_repetition } else { 2 };
                                        match item {
                                            Some(v) => leaf.push(6, repetition, Some(*v)),
                                            None => leaf.push(5, repetition, None),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            write::<Int32Type>(column, &leaf, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::TempDir;

    #[test]
    fn cache_stores_only_extracted_artifacts_and_hits_without_regenerating() {
        let dir = TempDir::new("extracted");
        let generated = Cell::new(0_u64);

        let first = load_cached(&dir.0, "sample", 2, true, &|index| {
            generated.set(generated.get() + 1);
            uniform_file(2 + usize::try_from(index).unwrap(), 1)
        });
        assert_eq!(generated.get(), 2);
        assert_eq!(first.footers.len(), 2);
        assert_eq!(first.file_sizes.len(), 2);
        assert_eq!(first.page_indexes.len(), 2);
        assert_eq!(first.page_index_file_sizes, first.file_sizes);

        // Verify no raw `.parquet` files are stored on disk, only extracted artifacts.
        let entry_dir = dir.0.join("sample");
        let mut names: Vec<String> = fs::read_dir(&entry_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "0000.footer",
                "0000.page_index",
                "0001.footer",
                "0001.page_index",
                "sizes.txt",
            ]
        );

        // Cache hit with `include_page_index = false` skips page indexes and does not call write.
        let footers_only = load_cached(&dir.0, "sample", 2, false, &|_| {
            panic!("should hit cache without regenerating")
        });
        assert_eq!(footers_only.footers, first.footers);
        assert_eq!(footers_only.file_sizes, first.file_sizes);
        assert_eq!(footers_only.page_indexes, Vec::<Vec<u8>>::new());
        assert_eq!(footers_only.page_index_file_sizes, Vec::<u64>::new());

        // Cache hit with `include_page_index = true` returns identical page indexes without write.
        let second = load_cached(&dir.0, "sample", 2, true, &|_| {
            panic!("should hit cache without regenerating")
        });
        assert_eq!(second.footers, first.footers);
        assert_eq!(second.file_sizes, first.file_sizes);
        assert_eq!(second.page_indexes, first.page_indexes);
        assert_eq!(second.page_index_file_sizes, first.page_index_file_sizes);
    }

    #[test]
    fn staging_dir_cleans_up_on_drop_without_commit() {
        let dir = TempDir::new("staging");
        let partial = dir.0.join("sample.partial-test");
        {
            let staging = StagingDir::create(partial.clone());
            fs::write(staging.path().join("0000.footer"), b"abc").unwrap();
            assert!(partial.is_dir());
        }
        assert!(
            !partial.exists(),
            "uncommitted staging directory should be removed on drop"
        );
    }

    #[test]
    fn prepare_cache_dir_prunes_stale_same_mode_and_legacy_dirs() {
        let base = TempDir::new("prune");
        let legacy_dir = base.0.join("0123456789abcdef");
        let stale_smoke = base.0.join("smoke-aaaaaaaaaaaaaaaa");
        let other_mode_full = base.0.join("full-bbbbbbbbbbbbbbbb");
        let unrelated_dir = base.0.join("notes");
        for path in [&legacy_dir, &stale_smoke, &other_mode_full, &unrelated_dir] {
            fs::create_dir_all(path).unwrap();
        }

        let fresh = prepare_cache_dir(&base.0, "smoke-1111111111111111", "smoke-");
        assert!(fresh.is_dir());
        assert!(!legacy_dir.exists(), "legacy v1 cache dir should be pruned");
        assert!(
            !stale_smoke.exists(),
            "stale same-mode cache dir should be pruned"
        );
        assert!(
            other_mode_full.is_dir(),
            "other mode's cache dir should be preserved"
        );
        assert!(
            unrelated_dir.is_dir(),
            "unrelated directories should not be deleted"
        );
    }
}
