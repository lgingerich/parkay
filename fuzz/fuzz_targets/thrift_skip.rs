//! `skip` over arbitrary bytes, from a type chosen by the first byte: it must never
//! panic, and the position must stay within the input whether it succeeds or fails.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::thrift::CompactReader;

fuzz_target!(|data: &[u8]| {
    let Some((ty, input)) = common::split(data) else {
        return;
    };
    let mut reader = CompactReader::new(input);
    let _ = reader.skip(ty);
    assert!(reader.position() <= input.len());
});
