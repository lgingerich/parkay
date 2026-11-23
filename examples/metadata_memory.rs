//! Prints how much memory retained metadata takes, for each benchmark corpus entry: the
//! footer's size, what parkay keeps (the metadata and the schema), and what arrow-rs
//! keeps for the same footer decoded with its default options.
//!
//! Run with `cargo run --release --example metadata_memory`.

#![expect(
    clippy::unwrap_used,
    reason = "a one-shot tool that panics on bad fixtures"
)]

use std::path::Path;
use std::sync::Arc;

use bench_support::{corpus, footer_tail};
use parkay::Limits;
use parkay::metadata::{Opened, Schema, open};
use parquet::file::metadata::{
    FileMetaData, ParquetMetaData, ParquetMetaDataOptions, ParquetMetaDataReader,
};
use parquet::schema::types::SchemaDescPtr;

/// Measures the heap bytes `ParquetMetaData::memory_size` attributes to a shared
/// `SchemaDescPtr` (arrow-rs's `HeapSize` trait is crate-private).
fn arrow_schema_heap_size(schema: SchemaDescPtr) -> usize {
    let probe = ParquetMetaData::new(
        FileMetaData::new(0, 0, None, None, schema, None),
        Vec::new(),
    );
    probe
        .memory_size()
        .saturating_sub(size_of::<ParquetMetaData>())
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!(
            "note: debug build uses the reduced SMOKE corpus; run with `--release` for full sizes"
        );
    }
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    println!(
        "{:<18} {:>12} {:>12} {:>12} {:>12}",
        "fixture", "footer", "parkay", "arrow-rs", "ratio"
    );
    for fixture in corpus::footers(&testdata) {
        let share_schema = fixture.name == "many_files" && fixture.footers.len() > 1;
        let mut parkay_schema: Option<Schema> = None;
        let mut arrow_schema: Option<SchemaDescPtr> = None;
        let (mut ours, mut theirs) = (0_usize, 0_usize);
        for (footer, file_size) in fixture.footers.iter().zip(&fixture.file_sizes) {
            let tail = footer_tail(footer);
            let Opened::File { metadata, schema } =
                open(&tail, *file_size, &Limits::new(), parkay_schema.as_ref()).unwrap()
            else {
                panic!("the tail holds the whole footer");
            };
            ours += metadata.memory_size();
            if let Some(decoded_schema) = schema {
                ours += decoded_schema.memory_size();
                if share_schema && parkay_schema.is_none() {
                    parkay_schema = Some(decoded_schema);
                }
            }
            let arrow_meta = if let Some(ref shared) = arrow_schema {
                let opts = ParquetMetaDataOptions::new().with_schema(Arc::clone(shared));
                ParquetMetaDataReader::decode_metadata_with_options(footer, Some(&opts)).unwrap()
            } else {
                let decoded = ParquetMetaDataReader::decode_metadata(footer).unwrap();
                if share_schema {
                    arrow_schema = Some(decoded.file_metadata().schema_descr_ptr());
                }
                decoded
            };
            theirs += arrow_meta.memory_size();
        }
        // `ParquetMetaData::memory_size()` counts its `Arc<SchemaDescriptor>` on every
        // file even when shared across `many_files`; subtract the N-1 duplicate copies
        // so both readers reflect one retained schema for the table.
        if let Some(shared) = arrow_schema {
            let duplicate_schemas = fixture.footers.len().saturating_sub(1);
            theirs = theirs.saturating_sub(arrow_schema_heap_size(shared) * duplicate_schemas);
        }
        let ratio = if ours == 0 {
            "n/a".to_owned()
        } else {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a ratio for display; the sizes are far below 2^52"
            )]
            let value = theirs as f64 / ours as f64;
            format!("{value:.2}x")
        };
        println!(
            "{:<18} {:>12} {:>12} {:>12} {:>12}",
            fixture.name,
            fixture.bytes(),
            ours,
            theirs,
            ratio
        );
    }
}
