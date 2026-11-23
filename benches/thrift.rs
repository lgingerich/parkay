//! Kernel benchmarks for the Thrift compact-protocol reader, over the footers and page
//! indexes of the benchmark corpus (`bench-support/src/corpus.rs`).
//!
//! These isolate the reader, which everything else is built on, so a small change in
//! it shows here before it is diluted in `open`. They are not part of the default run:
//! use `cargo xtask bench --bench thrift` when a profile points into the reader. Groups
//! are named `thrift_ENTRY` and `thrift_ENTRY_page_index`.
//!
//! Set `PARKAY_BENCH_DIR` to a directory of Parquet files (or pass
//! `cargo xtask bench --files DIR`) to add each file's footer and page index as
//! fixtures. A page index is a run of `ColumnIndex` and `OffsetIndex` structs, so its
//! `skip`, `arrow_skip`, and `walk` workloads go struct by struct to the end.
//!
//! Workloads, per fixture:
//! - `skip`: skip the whole footer as one struct. The floor for opening a file in
//!   parkay's lazy design, which walks the footer recording offsets.
//! - `skip_marking`: `skip`, also recording which bits decided where each value ends.
//!   Opening a file does this to the column chunks it later compares others with, so
//!   its cost over `skip` is what a comparison that never matches wastes.
//! - `arrow_skip`: the same with arrow-rs's reader (copied into `bench-support`).
//!   The like-for-like comparison for `skip`. Omitted on `*_page_index` fixtures
//!   because `ColumnIndex` begins with `list<bool> null_pages` and arrow-rs 60.1's
//!   `skip` consumes no bytes for bool list elements.
//! - `walk`: read every value with the `read_*` methods, without allocating. A proxy
//!   for decoding everything.
//! - `scan`: one compute-bound pass touching every byte. The floor for any parser that
//!   must look at each byte, and the run's control for CPU clock drift.
//! - `memcpy`: copy the footer bytes. The memory-bandwidth floor (saved with
//!   `--reference`).

#![expect(
    clippy::unwrap_used,
    reason = "benchmark setup should panic on missing or bad fixtures"
)]

use std::hint::black_box;
use std::path::Path;

use bench_support::arrow_thrift::{self, ThriftCompactInputProtocol, ThriftSliceInputProtocol};
use bench_support::fixtures::{Contents, Fixture};
use bench_support::{CONTROL_WORKLOAD, load_fixtures, run_control};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use parkay::thrift::{
    CompactReader, ElementType, FieldCursor, FieldType, Marks, SKIP_DEPTH_MAX, ThriftError,
};

/// Reads every value in a value of type `element_type`, returning a checksum so nothing is
/// optimized away. Maps never occur in Parquet and are skipped. Nesting is bounded by
/// `SKIP_DEPTH_MAX` so arbitrary `--files` inputs cannot overflow the stack.
fn walk(
    reader: &mut CompactReader<'_>,
    element_type: ElementType,
    depth: u16,
) -> Result<u64, ThriftError> {
    if depth >= SKIP_DEPTH_MAX {
        return Err(ThriftError::DepthExceeded {
            offset: u32::try_from(reader.position()).unwrap_or(u32::MAX),
            limit: SKIP_DEPTH_MAX,
        });
    }
    Ok(match element_type {
        ElementType::Bool => u64::from(reader.read_bool_element()?),
        ElementType::I8 => u64::from(reader.read_i8()?.cast_unsigned()),
        ElementType::I16 => u64::from(reader.read_i16()?.cast_unsigned()),
        ElementType::I32 => u64::from(reader.read_i32()?.cast_unsigned()),
        ElementType::I64 => reader.read_i64()?.cast_unsigned(),
        ElementType::Double => reader.read_double()?.to_bits(),
        ElementType::Binary => reader.read_binary()?.len() as u64,
        ElementType::List | ElementType::Set => {
            let header = reader.read_list_header()?;
            let mut sum = 0_u64;
            for _ in 0..header.size() {
                sum = sum.wrapping_add(walk(reader, header.element_type(), depth + 1)?);
            }
            sum
        }
        ElementType::Struct => {
            let mut sum = 0_u64;
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(reader)? {
                let value = match field.field_type().element_type() {
                    Some(element) => walk(reader, element, depth + 1)?,
                    None => u64::from(matches!(field.field_type(), FieldType::Bool(true))),
                };
                sum = sum
                    .wrapping_add(value)
                    .wrapping_add(u64::from(field.id().cast_unsigned()));
            }
            sum
        }
        ElementType::Map | ElementType::Uuid => {
            reader.skip(element_type)?;
            0
        }
    })
}

/// Skips structs until the buffer ends: one for a footer, many for a page index.
fn skip_footer(footer: &[u8]) -> usize {
    let mut reader = CompactReader::new(footer, 0);
    while reader.position() < footer.len() {
        reader.skip(ElementType::Struct).unwrap();
    }
    reader.position()
}

/// [`skip_footer`], marking. The marks are reused, as the pass that opens a file
/// reuses them from one column chunk to the next.
fn skip_marking_footer(footer: &[u8], marks: &mut Marks) -> usize {
    marks.clear();
    let mut reader = CompactReader::new(footer, 0);
    while reader.position() < footer.len() {
        reader.skip_marking(ElementType::Struct, marks).unwrap();
    }
    reader.position()
}

fn arrow_skip_footer(footer: &[u8]) -> usize {
    let mut reader = ThriftSliceInputProtocol::new(footer);
    while !reader.as_slice().is_empty() {
        reader.skip(arrow_thrift::FieldType::Struct).unwrap();
    }
    reader.as_slice().len()
}

fn walk_footer(footer: &[u8]) -> u64 {
    let mut reader = CompactReader::new(footer, 0);
    let mut sum = 0_u64;
    while reader.position() < footer.len() {
        sum = sum.wrapping_add(walk(&mut reader, ElementType::Struct, 0).unwrap());
    }
    sum
}

/// Runs `f` over every footer of the fixture.
fn each<T>(fixture: &Fixture, f: impl Fn(&[u8]) -> T) {
    for footer in &fixture.footers {
        black_box(f(black_box(footer)));
    }
}

fn bench_fixture(c: &mut Criterion, fixture: &Fixture) {
    let mut group = c.benchmark_group(format!("thrift_{}", fixture.name));
    group.throughput(Throughput::Bytes(fixture.bytes() as u64));
    group.bench_function("skip", |b| b.iter(|| each(fixture, skip_footer)));
    group.bench_function("skip_marking", |b| {
        let mut marks = Marks::new();
        b.iter(|| {
            for footer in &fixture.footers {
                black_box(skip_marking_footer(black_box(footer), &mut marks));
            }
            black_box(&marks);
        });
    });
    // The reference: run by `cargo xtask bench --bench thrift --reference`.
    // Skipped on page indexes: `ColumnIndex` starts with `list<bool> null_pages`, and
    // arrow-rs 60.1's `skip` consumes 0 bytes for bool list elements (so it misparses
    // single-page indexes via a cold fallback and panics on multi-page ones).
    if fixture.contents != Contents::PageIndex {
        group.bench_function("arrow_skip", |b| {
            b.iter(|| each(fixture, arrow_skip_footer));
        });
    }
    group.bench_function("walk", |b| b.iter(|| each(fixture, walk_footer)));
    group.bench_function(CONTROL_WORKLOAD, |b| {
        b.iter(|| run_control(&fixture.footers));
    });
    group.bench_function("memcpy", |b| {
        let mut scratch: Vec<Vec<u8>> = fixture.footers.iter().map(|f| vec![0; f.len()]).collect();
        b.iter(|| {
            for (dst, src) in scratch.iter_mut().zip(&fixture.footers) {
                dst.copy_from_slice(black_box(src));
                black_box(dst.as_slice());
            }
        });
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let all = load_fixtures(&testdata);
    for fixture in &all {
        bench_fixture(c, fixture);
    }
}

criterion_group!(thrift, benches);
criterion_main!(thrift);
