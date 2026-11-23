//! Opening a file from arbitrary footer bytes: nothing may panic, and what opens must
//! be consistent with itself.
//!
//! The input is a selector byte and then a footer. The eight bytes that end a file are
//! added here, so mutations need not keep a length and a magic consistent. The selector
//! picks the file size the footer is said to belong to.
//!
//! Checked on every footer that opens:
//! - everything can be read: the schema, the key-value pairs, every column chunk;
//! - one column extracted alone has the same facts as that column among all of them;
//! - opening again with the file's own schema supplied gives the same metadata.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::Limits;
use parkay::metadata::{ChunkFacts, ChunkSlot, Columns, Metadata, Opened, Request, open};

/// Columns checked one at a time. A fuzzed schema can claim far more.
const COLUMNS_ALONE_MAX: u32 = 4;

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
    let limits = Limits::new();
    let Ok(Opened::File { metadata, schema }) = open(&tail, file_size, &limits, None) else {
        return;
    };
    let schema = schema.expect("no schema was supplied, so one was decoded");
    assert_eq!(metadata.column_count(), schema.column_count());
    for index in 0..schema.node_count() {
        let node = schema.node(index).expect("the index is within the tree");
        drop((node.name(), node.path(), node.logical_type()));
    }
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

    let mut all = ChunkFacts::new();
    let all_result = metadata.extract(&everything(&metadata), &mut all);
    if all_result.is_ok() {
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
});
