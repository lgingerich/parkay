//! The default benchmarks: opening a file and planning a query from its metadata, over
//! every file of the benchmark corpus, beside arrow-rs doing the same.
//!
//! Run through `cargo xtask bench` (see `cargo xtask bench --help`). The corpus is
//! described in `bench-support/src/corpus.rs`. To see where a workload's time goes,
//! profile it with `cargo xtask profile --entry NAME --workload NAME --breakdown`
//! rather than adding a benchmark per step.
//!
//! Workloads, per corpus entry. Each starts from the bytes at the end of the file, as
//! a caller does, and so includes copying the footer.
//! - `open`: open the file: the footer directory, the schema, the file-level facts.
//! - `open_narrow`: open, then extract the locations and statistics of three columns
//!   (first, middle, last) in every row group. A typical query's planning.
//! - `open_all`: open, then extract the locations and statistics of every column. The
//!   lazy design's worst case.
//! - `open_locations`: open, then extract the location of every column and nothing
//!   else. What a scan of every column needs before it reads any data.
//! - `open_encodings`: `open_locations` with the data page encodings as well, which a
//!   reader that wants dictionary codes asks for. The difference between the two is what
//!   reaching `encoding_stats` costs.
//! - `arrow_open`, `arrow_open_narrow`, `arrow_open_all`: arrow-rs's metadata decode
//!   configured to do the least work that returns the same thing: no statistics, the
//!   three columns' statistics, every column's statistics. Encoding and size statistics
//!   are skipped throughout. These are the reference; `cargo xtask bench` runs them
//!   only with `--reference`.
//! - `scan`: one compute-bound pass touching every byte of the footers. The floor for
//!   any parser that looks at each byte, and the run's control for CPU clock drift.
//!
//! `open_narrow` and `open_all` intentionally reuse a single `ChunkFacts` buffer across
//! iterations (created outside `b.iter()`) to measure parkay's steady-state sans-I/O
//! extraction API, whereas `arrow_open_*` allocates fresh decoded metadata on every
//! iteration because arrow-rs has no reusable output buffer.
//!
//! In `many_files`, every workload is given the table's schema, as a reader planning
//! over many files of one table would.

#![expect(
    clippy::unwrap_used,
    reason = "benchmark setup should panic on missing or bad fixtures"
)]

use std::hint::black_box;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use bench_support::fixtures::Fixture;
use bench_support::{CONTROL_WORKLOAD, footer_tail, load_footer_fixtures, run_control};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use parkay::Limits;
use parkay::metadata::{ChunkFacts, Columns, Metadata, Opened, Request, Schema, open};
use parquet::file::metadata::{
    ParquetMetaDataOptions, ParquetMetaDataReader, ParquetStatisticsPolicy,
};
use parquet::schema::types::SchemaDescPtr;

/// One file of a corpus entry, prepared for every workload.
struct Case<'a> {
    /// The footer alone, which is what arrow-rs decodes.
    footer: &'a [u8],
    /// The end of the file: the footer, its length, and the magic.
    tail: Vec<u8>,
    file_size: u64,
    row_group_count: u32,
    /// The first, middle, and last leaf columns, or fewer in a file with fewer.
    narrow: Vec<u32>,
    arrow: ArrowOptions,
}

/// All files of one corpus entry, plus the single table schema shared across files when
/// the entry represents one table (`many_files`).
struct EntryCases<'a> {
    schema_shared: Option<Schema>,
    files: Vec<Case<'a>>,
}

/// arrow-rs's options for each workload.
struct ArrowOptions {
    open: ParquetMetaDataOptions,
    narrow: ParquetMetaDataOptions,
    all: ParquetMetaDataOptions,
}

impl<'a> EntryCases<'a> {
    fn new(fixture: &'a Fixture, share_schema: bool) -> Self {
        let mut schema_shared: Option<Schema> = None;
        let arrow_schema: Option<SchemaDescPtr> = if share_schema {
            fixture.footers.first().map(|first_footer| {
                let no_stats = arrow_no_statistics();
                ParquetMetaDataReader::decode_metadata_with_options(first_footer, Some(&no_stats))
                    .unwrap()
                    .file_metadata()
                    .schema_descr_ptr()
            })
        } else {
            None
        };
        let files = fixture
            .footers
            .iter()
            .zip(&fixture.file_sizes)
            .map(|(footer, &file_size)| {
                let tail = footer_tail(footer);
                let Opened::File { metadata, schema } =
                    open(&tail, file_size, &Limits::new(), schema_shared.as_ref()).unwrap()
                else {
                    panic!("the tail holds the whole footer");
                };
                if share_schema && schema_shared.is_none() {
                    schema_shared = schema;
                }
                let column_count = metadata.column_count();
                let mut narrow = vec![0, column_count / 2, column_count.saturating_sub(1)];
                narrow.dedup();
                narrow.retain(|column| *column < column_count);
                let with_schema = |options: ParquetMetaDataOptions| {
                    if let Some(ref shared) = arrow_schema {
                        options.with_schema(Arc::clone(shared))
                    } else {
                        options
                    }
                };
                let narrow_indices: Vec<usize> =
                    narrow.iter().map(|column| *column as usize).collect();
                let arrow = ArrowOptions {
                    open: with_schema(arrow_no_statistics()),
                    narrow: with_schema(arrow_no_statistics().with_column_stats_policy(
                        ParquetStatisticsPolicy::skip_except(&narrow_indices),
                    )),
                    all: with_schema(
                        arrow_no_statistics()
                            .with_column_stats_policy(ParquetStatisticsPolicy::KeepAll),
                    ),
                };
                Case {
                    footer: footer.as_slice(),
                    tail,
                    file_size,
                    row_group_count: metadata.row_group_count(),
                    narrow,
                    arrow,
                }
            })
            .collect();
        Self {
            schema_shared,
            files,
        }
    }
}

impl Case<'_> {
    fn open(&self, schema_shared: Option<&Schema>) -> Metadata {
        match open(
            black_box(self.tail.as_slice()),
            self.file_size,
            &Limits::new(),
            schema_shared,
        )
        .unwrap()
        {
            Opened::File { metadata, schema } => {
                black_box(schema);
                metadata
            }
            Opened::MissingBytes { .. } => panic!("the tail holds the whole footer"),
        }
    }

    fn narrow(&self) -> Request<'_> {
        Request::new(Columns::Indices(&self.narrow), 0..self.row_group_count)
            .with_location()
            .with_statistics()
    }

    fn locations(&self) -> Request<'_> {
        Request::new(Columns::All, 0..self.row_group_count).with_location()
    }

    fn all(&self) -> Request<'_> {
        Request::new(Columns::All, 0..self.row_group_count)
            .with_location()
            .with_statistics()
    }
}

/// arrow-rs options that skip every kind of statistics.
fn arrow_no_statistics() -> ParquetMetaDataOptions {
    ParquetMetaDataOptions::new()
        .with_column_stats_policy(ParquetStatisticsPolicy::SkipAll)
        .with_encoding_stats_policy(ParquetStatisticsPolicy::SkipAll)
        .with_size_stats_policy(ParquetStatisticsPolicy::SkipAll)
}

fn arrow_decode(footer: &[u8], options: &ParquetMetaDataOptions) {
    black_box(
        ParquetMetaDataReader::decode_metadata_with_options(black_box(footer), Some(options))
            .unwrap(),
    );
}

fn bench_entry(c: &mut Criterion, fixture: &Fixture) {
    // Only the built-in multi-file table entry shares a schema; an external
    // `--files` input named `many_files.parquet` has a single footer.
    let share_schema = fixture.name == "many_files" && fixture.footers.len() > 1;
    // Build cases lazily on first benchmark run so `--list` and filtered runs skip
    // untouched entries.
    let prepared: OnceLock<EntryCases<'_>> = OnceLock::new();
    let cases = || prepared.get_or_init(|| EntryCases::new(fixture, share_schema));

    let mut group = c.benchmark_group(&fixture.name);
    group.throughput(Throughput::Bytes(fixture.bytes() as u64));
    group.bench_function("open", |b| {
        let entry = cases();
        b.iter(|| {
            for file in &entry.files {
                drop(black_box(file.open(entry.schema_shared.as_ref())));
            }
        });
    });
    group.bench_function("open_narrow", |b| {
        let entry = cases();
        // Reused across iterations to measure steady-state extraction; arrow-rs allocates
        // fresh metadata on every `arrow_open_*` iteration.
        let mut facts = ChunkFacts::new();
        b.iter(|| {
            for file in &entry.files {
                file.open(entry.schema_shared.as_ref())
                    .extract(&file.narrow(), &mut facts)
                    .unwrap();
                black_box(&facts);
            }
        });
    });
    group.bench_function("open_all", |b| {
        let entry = cases();
        let mut facts = ChunkFacts::new();
        b.iter(|| {
            for file in &entry.files {
                file.open(entry.schema_shared.as_ref())
                    .extract(&file.all(), &mut facts)
                    .unwrap();
                black_box(&facts);
            }
        });
    });
    group.bench_function("open_locations", |b| {
        let entry = cases();
        let mut facts = ChunkFacts::new();
        b.iter(|| {
            for file in &entry.files {
                file.open(entry.schema_shared.as_ref())
                    .extract(&file.locations(), &mut facts)
                    .unwrap();
                black_box(&facts);
            }
        });
    });
    group.bench_function("open_encodings", |b| {
        let entry = cases();
        let mut facts = ChunkFacts::new();
        b.iter(|| {
            for file in &entry.files {
                file.open(entry.schema_shared.as_ref())
                    .extract(&file.locations().with_data_page_encodings(), &mut facts)
                    .unwrap();
                black_box(&facts);
            }
        });
    });
    group.bench_function("arrow_open", |b| {
        let entry = cases();
        b.iter(|| {
            for file in &entry.files {
                arrow_decode(file.footer, &file.arrow.open);
            }
        });
    });
    group.bench_function("arrow_open_narrow", |b| {
        let entry = cases();
        b.iter(|| {
            for file in &entry.files {
                arrow_decode(file.footer, &file.arrow.narrow);
            }
        });
    });
    group.bench_function("arrow_open_all", |b| {
        let entry = cases();
        b.iter(|| {
            for file in &entry.files {
                arrow_decode(file.footer, &file.arrow.all);
            }
        });
    });
    group.bench_function(CONTROL_WORKLOAD, |b| {
        b.iter(|| run_control(&fixture.footers));
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    for fixture in &load_footer_fixtures(&testdata) {
        bench_entry(c, fixture);
    }
}

criterion_group!(metadata, benches);
criterion_main!(metadata);
