//! Benchmarks for the Thrift compact-protocol reader over real and synthetic footers.
//!
//! Run through `cargo xtask bench` (see `cargo xtask help`), or directly with
//! `cargo bench --bench thrift --features internals`. Fixtures are in
//! `bench-support/src/fixtures.rs`.
//!
//! Set `PARKAY_BENCH_DIR` to a directory of Parquet files (or pass
//! `cargo xtask bench --files DIR`) to add each file's footer and page index as
//! fixtures. A page index is a run of `ColumnIndex` and `OffsetIndex` structs, so its
//! `skip`, `arrow_skip`, and `walk` workloads go struct by struct to the end.
//!
//! Workloads, per fixture:
//! - `skip`: skip the whole footer as one struct. The floor for opening a file in
//!   parkay's lazy design, which walks the footer recording offsets.
//! - `arrow_skip`: the same with arrow-rs's reader (copied into `bench-support`).
//!   The like-for-like comparison for `skip`.
//! - `walk`: read every value with the `read_*` methods, without allocating. A proxy
//!   for decoding everything.
//! - `scan`: one pass touching every byte (counting bytes with the high bit set). The
//!   floor for any parser that must look at each byte.
//! - `memcpy`: copy the footer bytes. The memory-bandwidth floor.
//! - `arrow_rs_decode`: arrow-rs's full metadata decode into owned structs. A
//!   reference point only: it does more work than `skip` or `walk`.

#![expect(
    clippy::unwrap_used,
    reason = "benchmark setup should panic on missing or bad fixtures"
)]

use std::hint::black_box;
use std::path::Path;

use bench_support::arrow_thrift::{self, ThriftCompactInputProtocol, ThriftSliceInputProtocol};
use bench_support::fixtures::{self, Contents, Fixture};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use parkay::thrift::{CompactReader, ElementType, FieldCursor, FieldType, ThriftError};
use parquet::file::metadata::ParquetMetaDataReader;

/// Reads every value in a value of type `ty`, returning a checksum so nothing is
/// optimized away. Maps never occur in Parquet and are skipped.
fn walk(reader: &mut CompactReader<'_>, ty: ElementType) -> Result<u64, ThriftError> {
    Ok(match ty {
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
                sum = sum.wrapping_add(walk(reader, header.element_type())?);
            }
            sum
        }
        ElementType::Struct => {
            let mut sum = 0_u64;
            let mut fields = FieldCursor::default();
            while let Some(field) = fields.next(reader)? {
                let value = match field.ty() {
                    FieldType::Bool(b) => u64::from(b),
                    FieldType::I8 => walk(reader, ElementType::I8)?,
                    FieldType::I16 => walk(reader, ElementType::I16)?,
                    FieldType::I32 => walk(reader, ElementType::I32)?,
                    FieldType::I64 => walk(reader, ElementType::I64)?,
                    FieldType::Double => walk(reader, ElementType::Double)?,
                    FieldType::Binary => walk(reader, ElementType::Binary)?,
                    FieldType::List => walk(reader, ElementType::List)?,
                    FieldType::Set => walk(reader, ElementType::Set)?,
                    FieldType::Struct => walk(reader, ElementType::Struct)?,
                    other => {
                        reader.skip_field(other)?;
                        0
                    }
                };
                sum = sum
                    .wrapping_add(value)
                    .wrapping_add(u64::from(field.id().cast_unsigned()));
            }
            sum
        }
        ElementType::Map | ElementType::Uuid => {
            reader.skip(ty)?;
            0
        }
    })
}

/// Skips structs until the buffer ends: one for a footer, many for a page index.
fn skip_footer(footer: &[u8]) -> usize {
    let mut reader = CompactReader::new(footer);
    while reader.position() < footer.len() {
        reader.skip(ElementType::Struct).unwrap();
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
    let mut reader = CompactReader::new(footer);
    let mut sum = 0_u64;
    while reader.position() < footer.len() {
        sum = sum.wrapping_add(walk(&mut reader, ElementType::Struct).unwrap());
    }
    sum
}

fn scan_footer(footer: &[u8]) -> usize {
    footer.iter().filter(|&&byte| byte & 0x80 != 0).count()
}

/// Runs `f` over every footer of the fixture.
fn each<T>(fixture: &Fixture, f: impl Fn(&[u8]) -> T) {
    for footer in &fixture.footers {
        black_box(f(black_box(footer)));
    }
}

fn bench_fixture(c: &mut Criterion, fixture: &Fixture) {
    let mut group = c.benchmark_group(&fixture.name);
    group.throughput(Throughput::Bytes(fixture.bytes() as u64));
    group.bench_function("skip", |b| b.iter(|| each(fixture, skip_footer)));
    group.bench_function("arrow_skip", |b| {
        b.iter(|| each(fixture, arrow_skip_footer));
    });
    group.bench_function("walk", |b| b.iter(|| each(fixture, walk_footer)));
    group.bench_function("scan", |b| b.iter(|| each(fixture, scan_footer)));
    let mut scratch: Vec<Vec<u8>> = fixture.footers.iter().map(|f| vec![0; f.len()]).collect();
    group.bench_function("memcpy", |b| {
        b.iter(|| {
            for (dst, src) in scratch.iter_mut().zip(&fixture.footers) {
                dst.copy_from_slice(black_box(src));
                black_box(&dst);
            }
        });
    });
    if fixture.contents == Contents::Footer {
        group.bench_function("arrow_rs_decode", |b| {
            b.iter(|| {
                each(fixture, |footer| {
                    ParquetMetaDataReader::decode_metadata(footer).unwrap()
                });
            });
        });
    }
    group.finish();
}

fn benches(c: &mut Criterion) {
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut all = fixtures::all(&testdata);
    if let Some(dir) = std::env::var_os("PARKAY_BENCH_DIR") {
        all.extend(fixtures::from_dir(Path::new(&dir)));
    }
    for fixture in &all {
        bench_fixture(c, fixture);
    }
}

criterion_group!(thrift, benches);
criterion_main!(thrift);
