//! `skip_marking` against `skip`, and the marks against what they claim.
//!
//! Marking must accept what `skip` accepts and end where it ends, including when
//! appending to non-empty `Marks` (and rolling back cleanly on error). Its marks say
//! which bits decided where the value ends, so each of these must still skip to its end:
//! - the value with every unmarked bit flipped or replaced;
//! - the value with a listed binary replaced by one of another length;
//! - the value with an integer made longer or a multi-byte integer shortened to 1 byte.
//!
//! The pass that opens a file relies on these invariants to pass over a column chunk by
//! comparing it with an earlier one instead of parsing it.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use parkay::thrift::{CompactReader, ElementType, Marks};

/// A valid Thrift struct containing one 1-byte binary (`field 1`) and one i32 (`field 2`),
/// used to exercise appending onto non-empty `Marks` and verifying rollback leaves no
/// dirty tail bytes in `Marks::mask`.
const PREFIX_STRUCT: [u8; 6] = [0x18, 0x01, 0x41, 0x15, 0x02, 0x00];

/// Asserts that `bytes` holds exactly one value of `element_type`.
fn assert_skips_to_its_end(bytes: &[u8], element_type: ElementType, what: &str) {
    let mut reader = CompactReader::new(bytes, 0);
    assert_eq!(reader.skip(element_type), Ok(()), "{what}");
    assert_eq!(reader.position(), bytes.len(), "{what}");
}

/// Verifies `skip_marking` when appending onto non-empty `Marks` from a non-zero input
/// offset, plus full rollback (including zeroing the dirty tail) when marking fails.
fn assert_append_and_rollback(
    data: &[u8],
    element_type: ElementType,
    fresh: &Marks,
    expected_end: Option<usize>,
) {
    let mut appended = Marks::new();
    let mut prefix_reader = CompactReader::new(&PREFIX_STRUCT, 0);
    prefix_reader
        .skip_marking(ElementType::Struct, &mut appended)
        .expect("prefix struct is valid");
    let prefix_mask = appended.mask().to_vec();
    let prefix_binaries = appended.binaries().to_vec();

    // `data[1..]` is `input`; starting at offset 1 exercises non-zero `input_start`.
    let mut reader = CompactReader::new(data, 1);
    let outcome = reader.skip_marking(element_type, &mut appended);

    if let Some(end) = expected_end {
        assert_eq!(
            outcome,
            Ok(()),
            "appended marking failed when fresh succeeded"
        );
        assert_eq!(reader.position(), 1 + end);
        assert_eq!(appended.mask().len(), prefix_mask.len() + end);
        assert_eq!(
            &appended.mask()[..prefix_mask.len()],
            prefix_mask.as_slice()
        );
        assert_eq!(&appended.mask()[prefix_mask.len()..], fresh.mask());
        let shift = u32::try_from(prefix_mask.len()).expect("prefix fits in u32");
        let expected_bins: Vec<u32> = prefix_binaries
            .iter()
            .copied()
            .chain(fresh.binaries().iter().map(|&pos| pos + shift))
            .collect();
        assert_eq!(appended.binaries(), expected_bins.as_slice());
    } else {
        assert!(
            outcome.is_err(),
            "appended marking succeeded when fresh failed"
        );
        assert_eq!(
            appended.mask(),
            prefix_mask.as_slice(),
            "failed append modified prior mask"
        );
        assert_eq!(
            appended.binaries(),
            prefix_binaries.as_slice(),
            "failed append modified prior binaries"
        );
        // Re-skipping `PREFIX_STRUCT` checks that the rollback zeroed any tail marks
        // written before the error (the `0x41` binary content byte must read as `CONTENT`).
        let mut verify_reader = CompactReader::new(&PREFIX_STRUCT, 0);
        verify_reader
            .skip_marking(ElementType::Struct, &mut appended)
            .expect("prefix struct still skips after rollback");
        assert_eq!(
            &appended.mask()[prefix_mask.len()..],
            prefix_mask.as_slice()
        );
    }
}

/// Checks `marks.binaries()` ordering/bounds/structure and tests replacing one binary
/// with a different length.
fn assert_binaries(value: &[u8], element_type: ElementType, marks: &Marks, choice: usize) {
    let mask = marks.mask();
    let mut prev: Option<usize> = None;
    for &pos in marks.binaries() {
        let pos = pos as usize;
        assert!(pos < value.len(), "binary position {pos} out of bounds");
        if let Some(prev) = prev {
            assert!(
                pos > prev,
                "binary positions not strictly ascending: {prev} then {pos}"
            );
        }
        assert_eq!(
            mask[pos],
            Marks::STRUCTURE,
            "binary start {pos} is not marked STRUCTURE"
        );
        prev = Some(pos);
    }

    if !marks.binaries().is_empty() {
        let start = marks.binaries()[choice % marks.binaries().len()] as usize;
        let mut binary = CompactReader::new(value, start);
        binary.read_binary().expect("a listed binary reads");
        let size = choice % 300;
        let mut other = value[..start].to_vec();
        if let Ok(small) = u8::try_from(size)
            && small < 0x80
        {
            other.push(small);
        } else {
            let low = u8::try_from(size & 0x7F).expect("masked to 7 bits") | 0x80;
            let high = u8::try_from(size >> 7).expect("size < 300 fits in two 7-bit bytes");
            other.extend([low, high]);
        }
        other.extend(std::iter::repeat_n(0xA5, size));
        other.extend(&value[binary.position()..]);
        assert_skips_to_its_end(&other, element_type, "a binary of another length");
    }
}

/// Tests lengthening a varint and shortening a multi-byte varint to 1 byte.
fn assert_varints(value: &[u8], element_type: ElementType, mask: &[u8], choice: usize) {
    let mut ends = Vec::new();
    let mut multi_byte = Vec::new();
    let mut run_start: Option<usize> = None;
    for (index, (&byte, &mark)) in value.iter().zip(mask).enumerate() {
        if mark == Marks::VARINT {
            let start = *run_start.get_or_insert(index);
            if byte < 0x80 {
                ends.push(index);
                if index > start {
                    multi_byte.push((start, index));
                }
                run_start = None;
            }
        } else {
            assert_eq!(
                run_start, None,
                "VARINT run ended without a terminating byte"
            );
        }
    }
    assert_eq!(
        run_start, None,
        "VARINT run reached end of value without terminating"
    );

    // An integer, made longer: its last byte continues into padding.
    if !ends.is_empty() {
        let last = ends[choice % ends.len()];
        let mut other = value[..last].to_vec();
        other.push(value[last] | 0x80);
        other.extend(std::iter::repeat_n(0x80, choice % 12));
        other.push(0x00);
        other.extend(&value[last + 1..]);
        assert_skips_to_its_end(&other, element_type, "an integer made longer");
    }

    // A multi-byte integer, shortened to a 1-byte varint (`0x00`).
    if !multi_byte.is_empty() {
        let (start, last) = multi_byte[choice % multi_byte.len()];
        let mut other = Vec::with_capacity(value.len() - (last - start));
        other.extend_from_slice(&value[..start]);
        other.push(0x00);
        other.extend_from_slice(&value[last + 1..]);
        assert_skips_to_its_end(
            &other,
            element_type,
            "a multi-byte integer shortened to 1 byte",
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((element_type, input)) = common::split(data) else {
        return;
    };
    let mut skipped = CompactReader::new(input, 0);
    let skip = skipped.skip(element_type);
    let mut marked = CompactReader::new(input, 0);
    let mut marks = Marks::new();
    let marking = marked.skip_marking(element_type, &mut marks);
    assert_eq!(marking, skip, "marking and skipping disagree on the input");
    if skip.is_err() {
        assert!(marks.mask().is_empty(), "a failed marking left marks");
        assert!(
            marks.binaries().is_empty(),
            "a failed marking left binaries"
        );
        assert_append_and_rollback(data, element_type, &marks, None);
        return;
    }
    let end = skipped.position();
    assert_eq!(marked.position(), end);
    assert_eq!(marks.mask().len(), end);
    assert_append_and_rollback(data, element_type, &marks, Some(end));

    let value = &input[..end];
    let mask = marks.mask();

    // Flip every single unmarked bit.
    let flipped: Vec<u8> = value
        .iter()
        .zip(mask)
        .map(|(&byte, &mark)| byte ^ !mark)
        .collect();
    assert_skips_to_its_end(
        &flipped,
        element_type,
        "flipping every unmarked bit changed where the value ends",
    );

    // Every marked bit from the value, every other bit from the input read backwards.
    let noisy: Vec<u8> = value
        .iter()
        .zip(mask)
        .zip(input.iter().rev())
        .map(|((byte, mask), noise)| (byte & mask) | (noise & !mask))
        .collect();
    assert_skips_to_its_end(
        &noisy,
        element_type,
        "the mask misses a bit that decides the end",
    );

    // A number from the input to choose with.
    let choice = input.iter().fold(0_usize, |sum, &byte| {
        sum.wrapping_mul(31).wrapping_add(usize::from(byte))
    });

    assert_binaries(value, element_type, &marks, choice);
    assert_varints(value, element_type, mask, choice);
});
