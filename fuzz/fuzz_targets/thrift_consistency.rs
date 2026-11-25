//! `skip` against the `read_*` methods and `skip_field`: whenever decoding a value with
//! the read API succeeds, `skip` (and `FieldCursor` + `skip_field` for structs) must also
//! succeed and end at exactly the same byte.
//!
//! The reverse need not hold: `skip` checks structure but not the values it discards
//! (integer widths, bool bytes, field ids), so it accepts some inputs the read API
//! rejects.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::thrift::{CompactReader, ElementType, FieldCursor, ThriftError};

fuzz_target!(|data: &[u8]| {
    let Some((element_type, input)) = common::split(data) else {
        return;
    };
    let mut decoded = CompactReader::new(input, 0);
    let walked = common::walk(&mut decoded, element_type, 0);
    assert!(decoded.position() <= input.len());

    let mut skipped = CompactReader::new(input, 0);
    let skip = skipped.skip(element_type);
    assert!(skipped.position() <= input.len());

    if walked.is_some() {
        assert_eq!(skip, Ok(()), "read API accepted what skip rejected");
        assert_eq!(
            skipped.position(),
            decoded.position(),
            "skip ended elsewhere"
        );
    }

    // For top-level structs, exercise `FieldCursor::next` + `CompactReader::skip_field`
    // directly so the inline integer fast path and `skip_non_scalar` dispatch are covered.
    if element_type == ElementType::Struct {
        let mut by_field = CompactReader::new(input, 0);
        let mut cursor = FieldCursor::default();
        let field_walk: Result<(), ThriftError> = (|| {
            while let Some(field) = cursor.next(&mut by_field)? {
                by_field.skip_field(field.field_type())?;
            }
            Ok(())
        })();
        assert!(by_field.position() <= input.len());
        if walked.is_some() {
            assert_eq!(
                field_walk,
                Ok(()),
                "read API accepted what skip_field rejected"
            );
            assert_eq!(
                by_field.position(),
                decoded.position(),
                "skip_field ended elsewhere"
            );
        }
        // `FieldCursor` unwraps the outer struct, so `skip_field` starts one depth level
        // shallower than `skip(ElementType::Struct)` and may accept depth 65 where `skip`
        // hits `DepthExceeded { limit: 64 }`.
        if field_walk.is_ok() && !matches!(skip, Err(ThriftError::DepthExceeded { .. })) {
            assert_eq!(skip, Ok(()), "skip_field accepted what skip rejected");
            assert_eq!(
                skipped.position(),
                by_field.position(),
                "skip and skip_field ended at different bytes"
            );
        }
    }
});
