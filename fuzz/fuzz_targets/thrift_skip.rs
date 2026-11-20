//! Skips one value of a fuzzer-chosen type over arbitrary bytes. `skip` validates
//! every value it passes over, so this drives the whole compact-protocol reader.

#![no_main]

use libfuzzer_sys::fuzz_target;
use parkay::thrift::{CompactReader, ElementType};

const TYPES: [ElementType; 12] = [
    ElementType::Struct,
    ElementType::List,
    ElementType::Set,
    ElementType::Map,
    ElementType::Binary,
    ElementType::Bool,
    ElementType::I8,
    ElementType::I16,
    ElementType::I32,
    ElementType::I64,
    ElementType::Double,
    ElementType::Uuid,
];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let ty = TYPES[usize::from(selector) % TYPES.len()];
    let mut reader = CompactReader::new(input);
    if reader.skip(ty).is_ok() {
        assert!(reader.position() <= input.len());
    }
});
