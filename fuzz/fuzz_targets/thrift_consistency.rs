//! `skip` against the `read_*` methods: whenever decoding a value with the read API
//! succeeds, `skip` must also succeed and end at exactly the same byte.
//!
//! The reverse need not hold: `skip` checks structure but not the values it discards
//! (integer widths, bool bytes, field ids), so it accepts some inputs the read API
//! rejects.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::thrift::CompactReader;

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
});
