//! Prints what the benchmark footers are made of: counts of each value kind, varint
//! lengths, binary lengths, list sizes, and where the bytes go. Used to choose
//! realistic synthetic workloads and to explain profiles.
//!
//! Run with `cargo run --release --example footer_stats --features internals -- [FIXTURE...]`.

#![expect(
    clippy::unwrap_used,
    reason = "a reporting tool that panics on bad fixtures"
)]
#![expect(
    clippy::cast_precision_loss,
    reason = "counts become display percentages"
)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use bench_support::corpus;
use parkay::thrift::{CompactReader, ElementType, FieldCursor, SKIP_DEPTH_MAX, ThriftError};

const BUCKET_LABELS: [&str; 7] = ["0", "1", "2-4", "5-16", "17-64", "65-256", ">256"];

#[derive(Default)]
struct Buckets([u64; BUCKET_LABELS.len()]);

impl Buckets {
    fn record(&mut self, n: usize) {
        let index = match n {
            0 => 0,
            1 => 1,
            2..=4 => 2,
            5..=16 => 3,
            17..=64 => 4,
            65..=256 => 5,
            _ => 6,
        };
        self.0[index] += 1;
    }
}

impl fmt::Display for Buckets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (label, &count) in BUCKET_LABELS.iter().zip(&self.0) {
            if count == 0 {
                continue;
            }
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "{label}: {count}")?;
            first = false;
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Stats {
    /// Values of each kind, with the bytes they occupy (headers excluded).
    kinds: BTreeMap<&'static str, (u64, u64)>,
    /// Varint lengths in bytes, for integer values.
    varint_len: BTreeMap<usize, u64>,
    /// Binary lengths, bucketed in ascending numeric order.
    binary_len: Buckets,
    /// List sizes, bucketed in ascending numeric order.
    list_size: Buckets,
    fields: u64,
    field_header_bytes: u64,
    long_form_ids: u64,
    max_depth: usize,
}

impl Stats {
    fn kind(&mut self, name: &'static str, bytes: usize) {
        let entry = self.kinds.entry(name).or_default();
        entry.0 += 1;
        entry.1 += bytes as u64;
    }
}

fn walk(
    reader: &mut CompactReader<'_>,
    element_type: ElementType,
    depth: usize,
    s: &mut Stats,
) -> Result<(), ThriftError> {
    assert!(
        depth <= usize::from(SKIP_DEPTH_MAX),
        "container nesting depth exceeds SKIP_DEPTH_MAX ({SKIP_DEPTH_MAX})"
    );
    s.max_depth = s.max_depth.max(depth);
    let start = reader.position();
    match element_type {
        ElementType::I16 | ElementType::I32 | ElementType::I64 => {
            reader.read_i64()?;
            let len = reader.position() - start;
            *s.varint_len.entry(len).or_default() += 1;
            s.kind("integer", len);
        }
        ElementType::Binary => {
            let bytes = reader.read_binary()?;
            s.binary_len.record(bytes.len());
            s.kind("binary", reader.position() - start);
        }
        ElementType::List | ElementType::Set => {
            let header = reader.read_list_header()?;
            s.list_size.record(header.size() as usize);
            s.kind("list header", reader.position() - start);
            for _ in 0..header.size() {
                walk(reader, header.element_type(), depth + 1, s)?;
            }
        }
        ElementType::Struct => {
            s.kind("struct", 0);
            let mut fields = FieldCursor::default();
            loop {
                let header_start = reader.position();
                let Some(field) = fields.next(reader)? else {
                    s.field_header_bytes += 1;
                    break;
                };
                s.fields += 1;
                let header_len = reader.position() - header_start;
                s.field_header_bytes += header_len as u64;
                if header_len > 1 {
                    s.long_form_ids += 1;
                }
                match field.field_type().element_type() {
                    None => s.kind("bool field", 0),
                    Some(element) => walk(reader, element, depth + 1, s)?,
                }
            }
        }
        other => {
            reader.skip(other)?;
            s.kind("other", reader.position() - start);
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let filters: Vec<String> = std::env::args().skip(1).collect();
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let fixtures = corpus::all(&testdata);
    let mut matched = 0;
    for fixture in &fixtures {
        if !filters.is_empty() && !filters.iter().any(|f| f == &fixture.name) {
            continue;
        }
        matched += 1;
        let mut s = Stats::default();
        for buf in &fixture.footers {
            let mut reader = CompactReader::new(buf, 0);
            while reader.position() < buf.len() {
                walk(&mut reader, ElementType::Struct, 0, &mut s).unwrap();
            }
        }
        let total = fixture.bytes() as f64;
        let bytes_per_field = if s.fields == 0 {
            0.0
        } else {
            total / s.fields as f64
        };
        let header_pct = if total == 0.0 {
            0.0
        } else {
            100.0 * s.field_header_bytes as f64 / total
        };
        println!(
            "== {} ({} buffers, {} bytes)",
            fixture.name,
            fixture.footers.len(),
            fixture.bytes()
        );
        println!(
            "  fields {}  ({bytes_per_field:.1} bytes/field; headers {header_pct:.1}% of bytes; long-form ids {})  max depth {}",
            s.fields, s.long_form_ids, s.max_depth
        );
        for (kind, (count, bytes)) in &s.kinds {
            let pct = if total == 0.0 {
                0.0
            } else {
                100.0 * *bytes as f64 / total
            };
            println!("  {kind:<12} {count:>9}  {pct:>5.1}% of bytes");
        }
        let varints: u64 = s.varint_len.values().sum();
        if varints == 0 {
            println!("  varint lengths: none");
        } else {
            let lens: Vec<String> = s
                .varint_len
                .iter()
                .map(|(len, n)| format!("{len}B {:.1}%", 100.0 * *n as f64 / varints as f64))
                .collect();
            println!("  varint lengths: {}", lens.join(", "));
        }
        println!("  binary lengths: {}", s.binary_len);
        println!("  list sizes:     {}", s.list_size);
    }
    if matched == 0 {
        let available: Vec<&str> = fixtures.iter().map(|f| f.name.as_str()).collect();
        eprintln!(
            "footer_stats: no fixture matched {:?}; available: {}",
            filters,
            available.join(", ")
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
