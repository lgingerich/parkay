//! Cold diagnostic replay and rare container/fallback paths for Thrift skipping.

use super::{
    ElemLoopOutcome, Grow, MASK_GROW_LOOKAHEAD, skip_list_elems_window, skip_list_top_fast,
    skip_struct_top_fast,
};
use crate::thrift::SKIP_DEPTH_MAX;
use crate::thrift::errors::ThriftError;
use crate::thrift::marks::{BinariesWindow, Marker, Marks, Unmarked};
use crate::thrift::reader::{CompactReader, size_exceeds_input};
use crate::thrift::types::{ElementType, FieldType, wire_type};

impl CompactReader<'_> {
    /// Fast-path nested skip `depth` containers down, returning `None` on any error
    /// without invoking [`Self::skip_diagnose`]. Errors unwind straight back to the
    /// top-level [`Self::skip`] / [`Self::skip_marking`], which runs `skip_diagnose` at
    /// most once, preventing exponential re-scanning on deeply nested malformed inputs.
    pub(super) fn skip_nested<M: Marker>(
        &mut self,
        element_type: ElementType,
        depth: u16,
        marker: &mut M,
    ) -> Option<()> {
        // Depth checks stop containers at the limit, so nothing is called deeper.
        debug_assert!(depth <= SKIP_DEPTH_MAX);
        match element_type {
            ElementType::List | ElementType::Set => {
                self.position =
                    skip_list_top_fast(self.input, self.position, u32::from(depth), marker)?;
                Some(())
            }
            ElementType::Map => self.skip_map(depth, marker),
            ElementType::Struct => {
                if depth >= SKIP_DEPTH_MAX {
                    return None;
                }
                self.position =
                    skip_struct_top_fast(self.input, self.position, u32::from(depth), marker)?;
                Some(())
            }
            ElementType::Bool | ElementType::I8 => self.take_array::<1>().ok().map(drop),
            ElementType::I16 | ElementType::I32 | ElementType::I64 => self.skip_varint(marker).ok(),
            ElementType::Double => self.take_array::<8>().ok().map(drop),
            ElementType::Uuid => self.take_array::<16>().ok().map(drop),
            ElementType::Binary => self.skip_binary(marker).ok(),
        }
    }

    /// Cold error-reporting pass run at most once at top level when the fast skip path
    /// returns `None`. Replays validation linearly (never re-entering the fast path) to
    /// produce the exact [`ThriftError`] variant and byte offset without bloating the hot
    /// loop with error construction or sret stack frames.
    #[cold]
    #[inline(never)]
    pub(super) fn skip_diagnose(
        &mut self,
        element_type: ElementType,
        depth: u16,
    ) -> Result<(), ThriftError> {
        match element_type {
            ElementType::Struct => {
                self.check_depth(depth)?;
                loop {
                    let start = self.position;
                    let byte = self.read_byte()?;
                    let code = byte & 0x0F;
                    if code == wire_type::STOP {
                        return Ok(());
                    }
                    let field_type = FieldType::from_code(code, start)?;
                    if byte >> 4 == 0 {
                        self.skip_varint(&mut Unmarked)?;
                    }
                    if let Some(child_type) = field_type.element_type() {
                        self.skip_diagnose(child_type, depth + 1)?;
                    }
                }
            }
            ElementType::List | ElementType::Set => {
                self.check_depth(depth)?;
                let header = self.read_list_header()?;
                let size = header.size();
                match header.element_type() {
                    ElementType::Bool | ElementType::I8 => self.advance_checked(size, 1),
                    ElementType::I16 | ElementType::I32 | ElementType::I64 => {
                        for _ in 0..size {
                            self.skip_varint(&mut Unmarked)?;
                        }
                    }
                    ElementType::Double => self.advance_checked(size, 8),
                    ElementType::Uuid => self.advance_checked(size, 16),
                    ElementType::Binary => {
                        for _ in 0..size {
                            self.skip_binary(&mut Unmarked)?;
                        }
                    }
                    container @ (ElementType::List
                    | ElementType::Set
                    | ElementType::Map
                    | ElementType::Struct) => {
                        for _ in 0..size {
                            self.skip_diagnose(container, depth + 1)?;
                        }
                    }
                }
                Ok(())
            }
            ElementType::Map => {
                if let Some((size, key, value)) = self.read_map_header(depth)? {
                    for _ in 0..size {
                        self.skip_diagnose(key, depth + 1)?;
                        self.skip_diagnose(value, depth + 1)?;
                    }
                }
                Ok(())
            }
            ElementType::Bool | ElementType::I8 => self.take_array::<1>().map(drop),
            ElementType::I16 | ElementType::I32 | ElementType::I64 => {
                self.skip_varint(&mut Unmarked)
            }
            ElementType::Double => self.take_array::<8>().map(drop),
            ElementType::Uuid => self.take_array::<16>().map(drop),
            ElementType::Binary => self.skip_binary(&mut Unmarked),
        }
    }

    /// Reads and validates a map header (`size`, plus key/value types if non-empty),
    /// returning `Ok(None)` for an empty map.
    fn read_map_header(
        &mut self,
        depth: u16,
    ) -> Result<Option<(u32, ElementType, ElementType)>, ThriftError> {
        self.check_depth(depth)?;
        let header_start = self.position;
        let size = self.read_size(header_start)?;
        if size == 0 {
            return Ok(None);
        }
        let types_start = self.position;
        let types = self.read_byte()?;
        let key = ElementType::from_code(types >> 4, types_start)?;
        let value = ElementType::from_code(types & 0x0F, types_start)?;
        self.check_fits(
            size,
            key.encoded_len_min() + value.encoded_len_min(),
            header_start,
        )?;
        Ok(Some((size, key, value)))
    }

    /// Fast-path map skip: reads the header, marks its structure bytes, and skips entries
    /// via [`Self::skip_nested`] (never re-entering [`Self::skip_diagnose`]).
    #[cold]
    #[inline(never)]
    fn skip_map<M: Marker>(&mut self, depth: u16, marker: &mut M) -> Option<()> {
        let header_start = self.position;
        let header = self.read_map_header(depth).ok()?;
        marker.mark_cold(header_start, self.position, Marks::STRUCTURE);
        if let Some((size, key, value)) = header {
            for _ in 0..size {
                self.skip_nested(key, depth + 1, marker)?;
                self.skip_nested(value, depth + 1, marker)?;
            }
        }
        Some(())
    }

    /// Advances past `count` fixed-width elements of `width` bytes whose total length
    /// was already checked against the remaining input by [`Self::check_fits`].
    fn advance_checked(&mut self, count: u32, width: usize) {
        let bytes = usize::try_from(count).unwrap_or(0) * width;
        debug_assert!(bytes <= self.input.len() - self.position);
        self.position += bytes;
    }

    fn check_depth(&self, depth: u16) -> Result<(), ThriftError> {
        if depth >= SKIP_DEPTH_MAX {
            return Err(ThriftError::DepthExceeded {
                offset: self.offset(),
                limit: SKIP_DEPTH_MAX,
            });
        }
        Ok(())
    }

    /// Skips a binary value: its length varint and that many bytes. Like
    /// [`Self::read_binary`] but without building the slice.
    fn skip_binary<M: Marker>(&mut self, marker: &mut M) -> Result<(), ThriftError> {
        let length_start = self.position;
        let size = self.read_size(length_start)?;
        marker.mark_binary_cold(length_start, self.position);
        let remaining = self.input.len() - self.position;
        match usize::try_from(size) {
            Ok(len) if len <= remaining => {
                self.position += len;
                Ok(())
            }
            _ => Err(size_exceeds_input(length_start, size, remaining)),
        }
    }
}

/// Cold byte-by-byte varint skip returning `Option<usize>` in registers.
#[cold]
#[inline(never)]
pub(super) fn skip_varint_long_fast(input: &[u8], mut pos: usize) -> Option<usize> {
    while let Some(&byte) = input.get(pos) {
        pos += 1;
        if byte < 0x80 {
            return Some(pos);
        }
    }
    None
}

/// Cold fallback for a short-delta binary field when `mask_start + input.len() > u32::MAX`.
#[cold]
#[inline(never)]
pub(super) fn skip_field_binary_cold<M: Marker>(
    input: &[u8],
    header_pos: usize,
    marker: &mut M,
) -> Option<usize> {
    marker.mark_cold(header_pos, header_pos + 1, Marks::STRUCTURE);
    let mut reader = CompactReader::new(input, header_pos + 1);
    reader.skip_binary(marker).ok().map(|()| reader.position)
}

/// Cold fallback for a long-delta struct field (`byte >> 4 == 0`).
#[cold]
#[inline(never)]
pub(super) fn skip_long_delta_field_cold<M: Marker>(
    input: &[u8],
    field_start: usize,
    depth: u32,
    marker: &mut M,
) -> Option<usize> {
    let byte = *input.get(field_start)?;
    let field_type = FieldType::from_code(byte & 0x0F, field_start).ok()?;
    marker.mark_cold(field_start, field_start + 1, Marks::STRUCTURE);
    let mut reader = CompactReader::new(input, field_start + 1);
    reader.skip_varint(marker).ok()?;
    if let Some(element_type) = field_type.element_type() {
        let child_depth = u16::try_from(depth + 1).ok()?;
        reader.skip_nested(element_type, child_depth, marker)?;
    }
    Some(reader.position)
}

/// Cold fallback for `Map` fields inside [`skip_struct_fast`].
#[cold]
#[inline(never)]
pub(super) fn skip_map_fast_cold<M: Marker>(
    input: &[u8],
    pos: usize,
    depth: u32,
    marker: &mut M,
) -> Option<usize> {
    let depth = u16::try_from(depth).ok()?;
    let mut reader = CompactReader::new(input, pos);
    reader.skip_map(depth, marker).map(|()| reader.position)
}

/// Cold fallback for `List<List|Set|Map>` inside [`skip_struct_fast`] and
/// [`skip_list_top_fast`].
#[cold]
#[inline(never)]
pub(super) fn skip_nested_elements_fast_cold<M: Marker>(
    input: &[u8],
    pos: usize,
    code: u8,
    count: u32,
    depth: u32,
    marker: &mut M,
) -> Option<usize> {
    let element_type = ElementType::from_code(code, 0).ok()?;
    let child_depth = u16::try_from(depth).ok()?;
    let mut reader = CompactReader::new(input, pos);
    for _ in 0..count {
        reader.skip_nested(element_type, child_depth, marker)?;
    }
    Some(reader.position)
}

/// Cold fallback when a `List<Varint>` or `List<Binary>` crosses a mask or binaries window
/// boundary mid-list. Grows the needed buffer and finishes the remaining elements without
/// re-scanning earlier elements.
#[cold]
#[inline(never)]
pub(super) fn skip_list_tail_growing<M: Marker>(
    input: &[u8],
    mut pos: usize,
    code: u8,
    mut remaining: u32,
    mut grow: Grow,
    marker: &mut M,
) -> Option<usize> {
    if code == wire_type::BINARY && !marker.can_use_bins_window(input.len()) {
        let mut reader = CompactReader::new(input, pos);
        for _ in 0..remaining {
            reader.skip_binary(marker).ok()?;
        }
        return Some(reader.position);
    }
    while remaining > 0 {
        match grow {
            Grow::Bins => marker.grow_bins(),
            Grow::Mask => {
                if !marker.try_grow_mask(pos + MASK_GROW_LOOKAHEAD, input.len()) {
                    return None;
                }
            }
        }
        let (limit, mut mask, mut bins) = marker.window(pos, input.len());
        let orig_bins = bins.remaining();
        let in_win = &input[..limit];
        let outcome = if code == wire_type::BINARY {
            skip_list_elems_window::<true, M>(
                in_win,
                input.len(),
                pos,
                remaining,
                &mut mask,
                &mut bins,
            )
        } else {
            skip_list_elems_window::<false, M>(
                in_win,
                input.len(),
                pos,
                remaining,
                &mut mask,
                &mut bins,
            )
        };
        let written = orig_bins - bins.remaining();
        drop((mask, bins));
        marker.commit_bins(written);
        match outcome {
            ElemLoopOutcome::Done(end) => return Some(end),
            ElemLoopOutcome::NeedGrow {
                at,
                remaining: rem,
                grow: next_grow,
            } if matches!(next_grow, Grow::Bins) || limit < input.len() => {
                pos = at;
                remaining = rem;
                grow = next_grow;
            }
            ElemLoopOutcome::NeedGrow { .. } | ElemLoopOutcome::Fail => return None,
        }
    }
    Some(pos)
}
