//! Opening a file from arbitrary footer bytes: nothing may panic, and what opens must
//! be consistent with itself.
//!
//! The input is a selector byte and then a footer. The eight bytes that end a file are
//! added here, so mutations need not keep a length and a magic consistent. The selector
//! picks the file size the footer is said to belong to and exercises raw framing/trailer
//! paths as well.
//!
//! Checked on every footer that opens:
//! - raw trailer framing (`MissingBytes` two-step retry, limit errors, invalid trailers);
//! - everything can be read: the schema, all metadata/node accessors, key-value pairs,
//!   column orders, and every column chunk;
//! - all four `extract_mode` combinations (`location`/`statistics` on/off), partial
//!   row-group ranges, duplicate/reordered column indices, reused `ChunkFacts`, and
//!   out-of-bounds `ExtractError` validation;
//! - opening again with the file's own schema supplied, or with a different schema
//!   supplied, gives the same metadata.

#![no_main]

mod common;

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use parkay::Limits;
use parkay::metadata::{
    ChunkFacts, ChunkSlot, Columns, ExtractError, Metadata, Opened, Request, Schema, open,
};

/// Columns checked one at a time. A fuzzed schema can claim far more.
const COLUMNS_ALONE_MAX: u32 = 4;

/// A tiny valid 1-column Parquet footer used to obtain a distinct `Schema` for testing
/// mismatched shared-schema fallback in `open(..., Some(&other_schema))`.
const OTHER_FOOTER: [u8; 23] = [
    0x15, 0x02, // field 1: version = 1
    0x19, 0x2C, // field 2: schema = List<Struct>(2)
    0x48, 0x01, b'r', 0x15, 0x02, 0x00, // root: name="r", num_children=1
    0x15, 0x02, 0x25, 0x00, 0x18, 0x01, b'c', 0x00, // leaf: INT32, REQUIRED, name="c"
    0x16, 0x00, // field 3: num_rows = 0
    0x19, 0x0C, // field 4: row_groups = List<Struct>(0)
    0x00, // stop
];

fn other_schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let tail = common::footer_tail(&OTHER_FOOTER).expect("constant footer fits");
        let Ok(Opened::File { schema, .. }) = open(&tail, tail.len() as u64, &Limits::new(), None)
        else {
            panic!("constant OTHER_FOOTER must open");
        };
        schema.expect("no schema supplied")
    })
}

fn everything(metadata: &Metadata) -> Request<'static> {
    Request::new(Columns::All, 0..metadata.row_group_count())
        .with_location()
        .with_statistics()
}

/// Asserts that two extractions hold the same facts for the same slots.
fn assert_same_facts(first: &ChunkFacts, second: &ChunkFacts) {
    assert_eq!(first.row_group_count(), second.row_group_count());
    assert_eq!(first.column_count(), second.column_count());
    for row_group in 0..first.row_group_count() {
        for column in 0..first.column_count() {
            let slot = ChunkSlot { row_group, column };
            assert_eq!(first.location(slot), second.location(slot));
            assert_eq!(first.statistics(slot), second.statistics(slot));
        }
    }
}

/// Exercises `open`'s framing, trailer, partial-read (`MissingBytes`), and `Limits` paths
/// using raw and sliced bytes so those branches are covered alongside valid footers.
fn exercise_framing(selector: u8, footer: &[u8], tail: &[u8], file_size: u64) {
    let raw_size = match selector & 0x03 {
        0 => footer.len() as u64,
        1 => footer.len().saturating_sub(1) as u64,
        2 => (footer.len() as u64).saturating_add(16),
        _ => 4,
    };
    let tight_limits = Limits::new()
        .with_footer_size_max(u32::from(selector))
        .with_schema_depth_max(selector.min(8));
    let _ = open(footer, raw_size, &tight_limits, None);

    // Two-step partial tail: supplying only the 8-byte trailer of a non-empty footer must
    // ask for the missing footer bytes when the file is large enough.
    if !footer.is_empty() && tail.len() > 8 {
        let trailer_only = &tail[tail.len() - 8..];
        let step1 = open(trailer_only, file_size, &Limits::new(), None);
        assert!(
            matches!(
                step1,
                Ok(Opened::MissingBytes { start, end })
                    if end == file_size - 8 && start < end
            ),
            "8-byte trailer for non-empty footer should request missing footer bytes: {step1:?}"
        );
    }

    // Encrypted footer magic (`PARE`) and footer-longer-than-file checks.
    if selector == 0xFF && footer.len() >= 8 {
        let mut pare = footer[..8].to_vec();
        pare[4..8].copy_from_slice(b"PARE");
        let _ = open(&pare, 64, &Limits::new(), None);
    }
}

/// Exercises all four `extract_mode` combinations, partial row-group ranges,
/// duplicate/reversed column indices, `ChunkFacts` reuse, and out-of-bounds errors.
fn exercise_extract_modes(metadata: &Metadata, all: &ChunkFacts) {
    let rg_count = metadata.row_group_count();
    let col_count = metadata.column_count();
    let mut reused = ChunkFacts::new();

    // Location-only (`<true, false>`).
    let req_loc = Request::new(Columns::All, 0..rg_count).with_location();
    metadata
        .extract(&req_loc, &mut reused)
        .expect("location-only extract succeeds when full extract succeeds");
    for row_group in 0..rg_count {
        for column in 0..col_count {
            let slot = ChunkSlot { row_group, column };
            assert_eq!(reused.location(slot), all.location(slot));
            assert_eq!(reused.statistics(slot), None);
        }
    }

    // Statistics-only (`<false, true>`), reusing `reused`.
    let req_stats = Request::new(Columns::All, 0..rg_count).with_statistics();
    metadata
        .extract(&req_stats, &mut reused)
        .expect("statistics-only extract succeeds when full extract succeeds");
    for row_group in 0..rg_count {
        for column in 0..col_count {
            let slot = ChunkSlot { row_group, column };
            assert_eq!(reused.location(slot), None);
            assert_eq!(reused.statistics(slot), all.statistics(slot));
        }
    }

    // Neither location nor statistics (`<false, false>`), reusing `reused`.
    // Note: unlike the other modes, `<false, false>` does not early-exit on `wanted_fields`,
    // so it scans past `ColumnMetaData` and may reject trailing `ColumnChunk` fields (e.g.
    // `Encryption`) that a full extract stopped before reaching.
    let req_none = Request::new(Columns::All, 0..rg_count);
    if metadata.extract(&req_none, &mut reused).is_ok() {
        for row_group in 0..rg_count {
            for column in 0..col_count {
                let slot = ChunkSlot { row_group, column };
                assert_eq!(reused.location(slot), None);
                assert_eq!(reused.statistics(slot), None);
            }
        }
    }
    // Out-of-bounds slot lookup on `ChunkFacts` must return `None`.
    let oob_slot = ChunkSlot {
        row_group: rg_count,
        column: col_count,
    };
    assert_eq!(reused.location(oob_slot), None);
    assert_eq!(reused.statistics(oob_slot), None);

    // Partial row-group range + reversed/duplicate `Columns::Indices`.
    if rg_count >= 2 && col_count >= 2 {
        let cols = [col_count - 1, 0, col_count - 1];
        let req_sub = Request::new(Columns::Indices(&cols), 1..rg_count)
            .with_location()
            .with_statistics();
        metadata
            .extract(&req_sub, &mut reused)
            .expect("subrange extract succeeds");
        assert_eq!(reused.row_group_count(), rg_count - 1);
        assert_eq!(reused.column_count(), 3);
        for rg_offset in 0..(rg_count - 1) {
            for (col_slot, &orig_col) in cols.iter().enumerate() {
                let dst = ChunkSlot {
                    row_group: rg_offset,
                    column: u32::try_from(col_slot).expect("3 fits in u32"),
                };
                let src = ChunkSlot {
                    row_group: rg_offset + 1,
                    column: orig_col,
                };
                assert_eq!(reused.location(dst), all.location(src));
                assert_eq!(reused.statistics(dst), all.statistics(src));
            }
        }
    }
}

/// Checks out-of-bounds request validation on `Metadata::extract`.
fn exercise_extract_errors(metadata: &Metadata) {
    let rg_count = metadata.row_group_count();
    let col_count = metadata.column_count();
    let mut facts = ChunkFacts::new();

    if let Some(end) = rg_count.checked_add(1) {
        let bad_rg = Request::new(Columns::All, rg_count..end).with_location();
        assert!(matches!(
            metadata.extract(&bad_rg, &mut facts),
            Err(ExtractError::RowGroups { .. })
        ));
    }
    let bad_col = [col_count];
    let bad_col_req = Request::new(Columns::Indices(&bad_col), 0..rg_count).with_location();
    assert!(matches!(
        metadata.extract(&bad_col_req, &mut facts),
        Err(ExtractError::Column { .. })
    ));
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, footer)) = data.split_first() else {
        return;
    };
    let Some(tail) = common::footer_tail(footer) else {
        return;
    };
    let file_size = match selector % 3 {
        // Real seeds begin with selector 0, so map it to a large file size that lets
        // `extract()` pass `location_check` without mutating byte 0 first.
        0 => 1 << 40,
        1 => tail.len() as u64 + 4,
        _ => tail.len() as u64,
    };

    exercise_framing(selector, footer, &tail, file_size);

    let limits = Limits::new();
    let Ok(Opened::File { metadata, schema }) = open(&tail, file_size, &limits, None) else {
        return;
    };
    let schema = schema.expect("no schema was supplied, so one was decoded");
    assert_eq!(metadata.column_count(), schema.column_count());
    assert_eq!(metadata.file_size(), file_size);
    assert_eq!(metadata.footer_offset(), file_size - tail.len() as u64);
    assert!(metadata.memory_size() >= footer.len());
    assert!(schema.memory_size() > 0);
    for rg in 0..metadata.row_group_count() {
        assert!(metadata.row_group_row_count(rg).is_some());
        assert!(metadata.row_group_byte_size(rg).is_some());
    }
    assert_eq!(
        metadata.row_group_row_count(metadata.row_group_count()),
        None
    );
    assert_eq!(
        metadata.row_group_byte_size(metadata.row_group_count()),
        None
    );

    for index in 0..schema.node_count() {
        let node = schema.node(index).expect("the index is within the tree");
        assert_eq!(node.index(), index);
        drop((
            node.name(),
            node.path(),
            node.parent(),
            node.child_count(),
            node.physical_type(),
            node.type_length(),
            node.repetition(),
            node.logical_type(),
            node.field_id(),
            node.definition_level_max(),
            node.repetition_level_max(),
        ));
    }
    for col in 0..schema.column_count() {
        let leaf = schema.column(col).expect("leaf column exists");
        assert!(leaf.physical_type().is_some());
    }

    // Tight limits on an already-valid footer must reject cleanly when below its size/depth.
    if let Ok(footer_len) = u32::try_from(footer.len())
        && footer_len > 0
    {
        let too_small = Limits::new().with_footer_size_max(footer_len - 1);
        assert!(open(&tail, file_size, &too_small, None).is_err());
    }
    let shallow = Limits::new().with_schema_depth_max(1);
    let _ = open(&tail, file_size, &shallow, None);

    // Reading every pair must not panic; a malformed pair is an error, not a crash.
    let kv_first: Vec<_> = metadata
        .key_value_metadata()
        .map(|entry| entry.map(|pair| (pair.key(), pair.value())))
        .collect();
    assert!(kv_first.len() <= footer.len());
    // The same for the writer's name and the column orders, which are decoded on request.
    let created_by = metadata.created_by();
    let orders_first = metadata
        .column_orders()
        .map(|opt| opt.map(Iterator::collect::<Vec<_>>));
    if let Ok(Some(ref column_orders)) = orders_first {
        assert!(column_orders.len() <= footer.len());
    }

    exercise_extract_errors(&metadata);

    let mut all = ChunkFacts::new();
    let all_result = metadata.extract(&everything(&metadata), &mut all);
    if all_result.is_ok() {
        exercise_extract_modes(&metadata, &all);

        let mut alone = ChunkFacts::new();
        for column in 0..metadata.column_count().min(COLUMNS_ALONE_MAX) {
            let only = [column];
            let request = Request::new(Columns::Indices(&only), 0..metadata.row_group_count())
                .with_location()
                .with_statistics();
            metadata
                .extract(&request, &mut alone)
                .expect("a column extracts alone if it extracts among all");
            for row_group in 0..metadata.row_group_count() {
                let among = ChunkSlot { row_group, column };
                let single = ChunkSlot {
                    row_group,
                    column: 0,
                };
                assert_eq!(alone.location(single), all.location(among));
                assert_eq!(alone.statistics(single), all.statistics(among));
            }
        }
    }

    // The file's own schema, supplied: recognized by its bytes or decoded again, the
    // metadata must come out the same.
    let Ok(Opened::File {
        metadata: again, ..
    }) = open(&tail, file_size, &limits, Some(&schema))
    else {
        panic!("a file that opens must open again with its own schema");
    };
    assert_eq!(again.version(), metadata.version());
    assert_eq!(again.row_count(), metadata.row_count());
    assert_eq!(again.column_count(), metadata.column_count());
    assert_eq!(again.row_group_count(), metadata.row_group_count());
    assert_eq!(again.created_by(), created_by);
    let kv_again: Vec<_> = again
        .key_value_metadata()
        .map(|entry| entry.map(|pair| (pair.key(), pair.value())))
        .collect();
    assert_eq!(kv_again, kv_first);
    let orders_again = again
        .column_orders()
        .map(|opt| opt.map(Iterator::collect::<Vec<_>>));
    assert_eq!(orders_again, orders_first);
    let mut all_again = ChunkFacts::new();
    assert_eq!(
        again.extract(&everything(&again), &mut all_again),
        all_result
    );
    if all_result.is_ok() {
        assert_same_facts(&all, &all_again);
    }

    // A different schema supplied: must fall back to decoding the file's own schema (or
    // reuse it if bytes happen to match) and produce identical metadata.
    let Ok(Opened::File {
        metadata: with_other,
        ..
    }) = open(&tail, file_size, &limits, Some(other_schema()))
    else {
        panic!("a file that opens must open again with a different schema supplied");
    };
    assert_eq!(with_other.version(), metadata.version());
    assert_eq!(with_other.row_count(), metadata.row_count());
    assert_eq!(with_other.column_count(), metadata.column_count());
    assert_eq!(with_other.row_group_count(), metadata.row_group_count());
});
