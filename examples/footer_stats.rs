//! Prints what the benchmark footers are made of: counts of each value kind, varint
//! lengths, binary lengths, list sizes, and where the bytes go. Used to choose
//! realistic synthetic workloads and to explain profiles.
//!
//! Run with `cargo run --release --example footer_stats --features internals`.

#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a reporting tool that panics on bad fixtures"
)]
#![expect(
    clippy::cast_precision_loss,
    reason = "counts become display percentages"
)]

use std::collections::BTreeMap;
use std::path::Path;

use bench_support::fixtures;
use parkay::thrift::{CompactReader, ElementType, FieldCursor, FieldType, ThriftError};

#[derive(Default)]
struct Stats {
    /// Values of each kind, with the bytes they occupy (headers excluded).
    kinds: BTreeMap<&'static str, (u64, u64)>,
    /// Varint lengths in bytes, for integer values.
    varint_len: BTreeMap<usize, u64>,
    /// Binary lengths, bucketed.
    binary_len: BTreeMap<&'static str, u64>,
    /// List sizes, bucketed.
    list_size: BTreeMap<&'static str, u64>,
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

fn bucket(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2..=4 => "2-4",
        5..=16 => "5-16",
        17..=64 => "17-64",
        65..=256 => "65-256",
        _ => ">256",
    }
}

fn walk(
    r: &mut CompactReader<'_>,
    ty: ElementType,
    depth: usize,
    s: &mut Stats,
) -> Result<(), ThriftError> {
    s.max_depth = s.max_depth.max(depth);
    let start = r.position();
    match ty {
        ElementType::I16 | ElementType::I32 | ElementType::I64 => {
            r.read_i64()?;
            let len = r.position() - start;
            *s.varint_len.entry(len).or_default() += 1;
            s.kind("integer", len);
        }
        ElementType::Binary => {
            let bytes = r.read_binary()?;
            *s.binary_len.entry(bucket(bytes.len())).or_default() += 1;
            s.kind("binary", r.position() - start);
        }
        ElementType::List | ElementType::Set => {
            let header = r.read_list_header()?;
            *s.list_size
                .entry(bucket(header.size() as usize))
                .or_default() += 1;
            s.kind("list header", r.position() - start);
            for _ in 0..header.size() {
                walk(r, header.element_type(), depth + 1, s)?;
            }
        }
        ElementType::Struct => {
            s.kind("struct", 0);
            let mut fields = FieldCursor::default();
            loop {
                let header_start = r.position();
                let Some(field) = fields.next(r)? else {
                    s.field_header_bytes += 1;
                    break;
                };
                s.fields += 1;
                let header_len = r.position() - header_start;
                s.field_header_bytes += header_len as u64;
                if header_len > 1 {
                    s.long_form_ids += 1;
                }
                match field.ty() {
                    FieldType::Bool(_) => s.kind("bool field", 0),
                    other => {
                        let element = match other {
                            FieldType::I8 => ElementType::I8,
                            FieldType::I16 => ElementType::I16,
                            FieldType::I32 => ElementType::I32,
                            FieldType::I64 => ElementType::I64,
                            FieldType::Double => ElementType::Double,
                            FieldType::Binary => ElementType::Binary,
                            FieldType::List => ElementType::List,
                            FieldType::Set => ElementType::Set,
                            FieldType::Map => ElementType::Map,
                            FieldType::Struct => ElementType::Struct,
                            FieldType::Uuid => ElementType::Uuid,
                            FieldType::Bool(_) => unreachable!(),
                        };
                        walk(r, element, depth + 1, s)?;
                    }
                }
            }
        }
        other => {
            r.skip(other)?;
            s.kind("other", r.position() - start);
        }
    }
    Ok(())
}

fn main() {
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    for fixture in fixtures::all(&testdata) {
        let mut s = Stats::default();
        for footer in &fixture.footers {
            walk(
                &mut CompactReader::new(footer),
                ElementType::Struct,
                0,
                &mut s,
            )
            .unwrap();
        }
        let total = fixture.bytes() as f64;
        println!(
            "== {} ({} footers, {} bytes)",
            fixture.name,
            fixture.footers.len(),
            fixture.bytes()
        );
        println!(
            "  fields {}  ({:.1} bytes/field; headers {:.1}% of bytes; long-form ids {})  max depth {}",
            s.fields,
            total / s.fields as f64,
            100.0 * s.field_header_bytes as f64 / total,
            s.long_form_ids,
            s.max_depth
        );
        for (kind, (count, bytes)) in &s.kinds {
            println!(
                "  {kind:<12} {count:>9}  {:>5.1}% of bytes",
                100.0 * *bytes as f64 / total
            );
        }
        let varints: u64 = s.varint_len.values().sum();
        let lens: Vec<String> = s
            .varint_len
            .iter()
            .map(|(len, n)| format!("{len}B {:.1}%", 100.0 * *n as f64 / varints as f64))
            .collect();
        println!("  varint lengths: {}", lens.join(", "));
        println!("  binary lengths: {:?}", s.binary_len);
        println!("  list sizes:     {:?}", s.list_size);
    }
}
