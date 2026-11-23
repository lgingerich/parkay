//! parkay's `skip` against arrow-rs 60.1's `skip` (the copy in `bench-support`): when
//! both succeed, they must end at the same byte.
//!
//! Inputs are compared only when both readers accept them, since their rules for
//! rejecting input differ (parkay caps binary lengths at `i32::MAX`, for example).
//! Inputs with bool list elements or maps are excluded; see `common::arrow_comparable`.

#![no_main]

#[expect(
    dead_code,
    reason = "reuses arrow-rs Thrift decoder copy from bench-support"
)]
#[path = "../../bench-support/src/arrow_thrift.rs"]
mod arrow_thrift;
mod common;

use arrow_thrift::{FieldType, ThriftCompactInputProtocol, ThriftSliceInputProtocol};
use libfuzzer_sys::fuzz_target;
use parkay::thrift::{CompactReader, ElementType};

fn arrow_type(element_type: ElementType) -> FieldType {
    match element_type {
        ElementType::Bool => FieldType::BooleanTrue,
        ElementType::I8 => FieldType::Byte,
        ElementType::I16 => FieldType::I16,
        ElementType::I32 => FieldType::I32,
        ElementType::I64 => FieldType::I64,
        ElementType::Double => FieldType::Double,
        ElementType::Binary => FieldType::Binary,
        ElementType::List => FieldType::List,
        ElementType::Set => FieldType::Set,
        ElementType::Map => FieldType::Map,
        ElementType::Struct => FieldType::Struct,
        ElementType::Uuid => FieldType::Uuid,
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((element_type, input)) = common::split(data) else {
        return;
    };
    let mut ours = CompactReader::new(input, 0);
    if ours.skip(element_type).is_err() || !common::arrow_comparable(input, element_type) {
        return;
    }
    let mut theirs = ThriftSliceInputProtocol::new(input);
    if theirs.skip(arrow_type(element_type)).is_ok() {
        let theirs_end = input.len() - theirs.as_slice().len();
        assert_eq!(
            ours.position(),
            theirs_end,
            "parkay and arrow-rs ended at different bytes"
        );
    }
});
