//! Fast windowed Thrift value skipping (both plain and marking).
//!
//! Cold diagnostic replay and rare container fallbacks live in [`cold`].

mod cold;

use cold::{
    skip_field_binary_cold, skip_list_tail_growing, skip_long_delta_field_cold, skip_map_fast_cold,
    skip_nested_elements_fast_cold, skip_varint_long_fast,
};

use super::SKIP_DEPTH_MAX;
use super::errors::ThriftError;
use super::marks::{BinariesWindow, Marker, Marking, Marks, MaskWindow, Unmarked};
use super::reader::{CompactReader, eof};
use super::types::{ElementType, FieldType, wire_type};
use super::varint::read_size_fast_cold;

/// Lookahead bytes requested past the resume position when growing the marking mask, so
/// the next multi-byte header or varint does not immediately hit the window edge again.
/// Clamped to `input_len + 8` by [`Marking::grow_to`].
const MASK_GROW_LOOKAHEAD: usize = 64;

/// Maximum encoded length in bytes of a valid `u32` size varint (`ceil(32 / 7) == 5`).
/// When at least this many bytes are available in the window and `read_size_fast_cold`
/// returns `None`, the varint itself is malformed rather than truncated at the window edge.
const SIZE_VARINT_LEN_MAX: usize = 5;

/// Inline nesting depth handled iteratively inside [`skip_struct_fast`] without recursion
/// or window re-entry. Four entries cover 5 levels of nested structs
/// (`FileMetaData` -> `RowGroup` -> `ColumnChunk` -> `ColumnMetaData` -> `Statistics`).
const INLINE_STACK: usize = 4;

// `StructState::pop_frame` masks `*top` with `INLINE_STACK - 1` and depth with `0xFF`.
const _: () = assert!(INLINE_STACK.is_power_of_two());
const _: () = assert!(SKIP_DEPTH_MAX <= u8::MAX as u16);
const _: () = assert!(u32::BITS.div_ceil(7) as usize == SIZE_VARINT_LEN_MAX);

impl CompactReader<'_> {
    /// Skips the value of a struct field. A bool field's value is in its header, so
    /// skipping it consumes nothing.
    ///
    /// # Errors
    ///
    /// Returns any error [`CompactReader::skip`] would.
    #[inline]
    pub fn skip_field(&mut self, field_type: FieldType) -> Result<(), ThriftError> {
        let element_type = match field_type {
            // An integer is passed over here, without a call into the general skip:
            // most fields a decoder does not want are integers.
            FieldType::I16 | FieldType::I32 | FieldType::I64 => {
                return self.skip_varint(&mut Unmarked);
            }
            FieldType::Bool(_) => return Ok(()),
            FieldType::I8 => ElementType::I8,
            FieldType::Double => ElementType::Double,
            FieldType::Binary => ElementType::Binary,
            FieldType::List => ElementType::List,
            FieldType::Set => ElementType::Set,
            FieldType::Map => ElementType::Map,
            FieldType::Struct => ElementType::Struct,
            FieldType::Uuid => ElementType::Uuid,
        };
        // Skip on a local clone so taking `&mut` for the out-of-line call
        // does not spill the caller's reader to the stack on hot paths.
        let mut sub = self.clone();
        sub.skip_non_scalar(element_type)?;
        self.position = sub.position;
        Ok(())
    }

    /// Out-of-line skip entry for [`Self::skip_field`]'s non-scalar arm so callers'
    /// hot parsing loops never inline the full container dispatch.
    #[inline(never)]
    fn skip_non_scalar(&mut self, element_type: ElementType) -> Result<(), ThriftError> {
        self.skip(element_type)
    }

    /// Shared top-level fast skip dispatch for [`Self::skip`] and [`Self::skip_marking`].
    #[inline(always)]
    fn skip_fast_top<M: Marker>(
        input: &[u8],
        pos: usize,
        element_type: ElementType,
        marker: &mut M,
    ) -> Option<usize> {
        match element_type {
            ElementType::Struct => skip_struct_top_fast(input, pos, 0, marker),
            ElementType::List | ElementType::Set => skip_list_top_fast(input, pos, 0, marker),
            ElementType::Bool
            | ElementType::I8
            | ElementType::I16
            | ElementType::I32
            | ElementType::I64
            | ElementType::Double
            | ElementType::Binary
            | ElementType::Map
            | ElementType::Uuid => {
                let mut sub = CompactReader::new(input, pos);
                sub.skip_nested(element_type, 0, marker)
                    .map(|()| sub.position)
            }
        }
    }

    /// Skips one value of the given type.
    ///
    /// Skipping checks structure, not values. Type codes, container and binary sizes,
    /// and nesting depth are validated, because they decide where the value ends.
    /// Values that are thrown away are not: integers are not checked against their
    /// width, bool elements not checked for `0`/`1`/`2`, and field ids not computed.
    /// A value skipped here can still fail when read with the `read_*` methods.
    ///
    /// Containers are descended into at most [`SKIP_DEPTH_MAX`] levels deep.
    ///
    /// # Errors
    ///
    /// Returns an error if the input ends, a type code or size is invalid, or
    /// containers nest too deeply.
    pub fn skip(&mut self, element_type: ElementType) -> Result<(), ThriftError> {
        if let Some(end) =
            Self::skip_fast_top(self.input, self.position, element_type, &mut Unmarked)
        {
            self.position = end;
            return Ok(());
        }
        self.skip_diagnose(element_type, 0)
    }

    /// Skips one value as [`skip`](Self::skip) does, and appends to `marks` which bits
    /// of the value decided where it ends. See [`Marks`].
    ///
    /// This accepts exactly what `skip` accepts and ends where `skip` ends: it is the
    /// same code. It is slower, so it is for the few values that others are compared
    /// with.
    ///
    /// # Errors
    ///
    /// Returns any error [`skip`](Self::skip) would, and then leaves `marks` as it was.
    pub fn skip_marking(
        &mut self,
        element_type: ElementType,
        marks: &mut Marks,
    ) -> Result<(), ThriftError> {
        let input_start = self.position;
        let (mask_start, binaries_start) = marks.snapshot();
        let slice = &self.input[input_start..];
        let mut marking = Marking::new(marks, mask_start);
        let skipped =
            if let Some(consumed) = Self::skip_fast_top(slice, 0, element_type, &mut marking) {
                self.position = input_start + consumed;
                Ok(())
            } else {
                self.skip_diagnose(element_type, 0)
            };
        if skipped.is_ok() {
            marks.finish_skip(mask_start, self.position - input_start);
        } else {
            marks.rollback_skip(mask_start, binaries_start);
        }
        skipped
    }

    /// Skips a varint without decoding or width-checking it: everything up to and
    /// including the first byte without the continuation bit.
    // Always inlined so `skip_field` skips integer fields without a call.
    #[inline(always)]
    fn skip_varint<M: Marker>(&mut self, marker: &mut M) -> Result<(), ThriftError> {
        let rest = &self.input[self.position..];
        // Fast path: most varints in footers are one byte.
        if rest.first().is_some_and(|&byte| byte < 0x80) {
            marker.mark_cold(self.position, self.position + 1, Marks::VARINT);
            self.position += 1;
            return Ok(());
        }
        let last = rest
            .iter()
            .position(|&byte| byte & 0x80 == 0)
            .ok_or_else(|| eof(self.input.len(), 1))?;
        marker.mark_cold(self.position, self.position + last + 1, Marks::VARINT);
        self.position += last + 1;
        debug_assert!(self.position <= self.input.len());
        Ok(())
    }
}

/// Which marking buffer exhausted its current window.
#[derive(Clone, Copy)]
enum Grow {
    Mask,
    Bins,
}

/// Marks an error return in [`skip_struct_fast`] as cold so LLVM lays out hot paths as
/// straight-line fallthrough rather than jumping over cold `None` blocks.
#[cold]
fn skip_fail<T>() -> Option<T> {
    None
}

/// Skips one varint inside `in_win`, returning `Some(new_pos)` in registers or `None` if
/// `in_win` ends before the terminating byte. When `WITH_HEADER` is `true`, `start` holds
/// the 1-byte struct field header and the varint begins at `start + 1`; both are marked
/// together, and nothing is written on `None`.
#[inline(always)]
fn skip_varint_fast<const WITH_HEADER: bool, W: MaskWindow>(
    in_win: &[u8],
    start: usize,
    mask: &mut W,
) -> Option<usize> {
    let pos = if WITH_HEADER { start + 1 } else { start };
    let Some(&first) = in_win.get(pos) else {
        return skip_fail();
    };
    // Note: the one-byte branch is load-bearing. A predicted branch lets the next field's
    // header load start before this byte arrives; a branchless 8-byte SWAR length (one
    // `ldr`, mask, `rbit`/`clz`) put that chain on the position dependency of every field
    // and was 40-57% slower on every `thrift` skip benchmark (quick, partial run, 2026-11-25).
    if first < 0x80 {
        if WITH_HEADER {
            mask.write_pair::<{ Marks::STRUCTURE }, { Marks::VARINT }>(start);
        } else {
            mask.write_byte::<{ Marks::VARINT }>(start);
        }
        return Some(pos + 1);
    }
    if let Some(chunk) = in_win.get(pos + 1..).and_then(|r| r.first_chunk::<4>()) {
        let term = !u32::from_le_bytes(*chunk) & 0x8080_8080;
        let tz = term.trailing_zeros();
        if tz < 32 {
            let idx = (tz >> 3) as usize;
            mask.write_swar::<WITH_HEADER>(start, idx);
            return Some(pos + 2 + idx);
        }
        let end = skip_varint_long_fast(in_win, pos + 5)?;
        mask.write_run::<WITH_HEADER, { Marks::VARINT }>(start, end);
        return Some(end);
    }
    let end = skip_varint_long_fast(in_win, pos + 1)?;
    mask.write_run::<WITH_HEADER, { Marks::VARINT }>(start, end);
    Some(end)
}

/// Outcome of skipping one element inside a window. Distinguishes buffer exhaustion from
/// genuine malformed-input errors.
#[derive(Clone, Copy)]
enum ElemStep {
    Done(usize),
    NeedGrow(Grow),
    Fail,
}

/// Skips one binary value inside a window. When `WITH_HEADER` is `true`, `start` holds
/// the 1-byte struct field header and the binary length begins at `start + 1`; both are
/// marked together and left untouched on `NeedGrow`.
#[inline(always)]
fn skip_binary_window<const WITH_HEADER: bool, W: MaskWindow, B: BinariesWindow>(
    in_win: &[u8],
    input_len: usize,
    start: usize,
    mask: &mut W,
    bins: &mut B,
) -> ElemStep {
    let pos = if WITH_HEADER { start + 1 } else { start };
    let Some(&first) = in_win.get(pos) else {
        return ElemStep::NeedGrow(Grow::Mask);
    };
    let (size, after_len) = if first < 0x80 {
        if !bins.try_record(pos) {
            return ElemStep::NeedGrow(Grow::Bins);
        }
        if WITH_HEADER {
            mask.write_pair::<{ Marks::STRUCTURE }, { Marks::STRUCTURE }>(start);
        } else {
            mask.write_byte::<{ Marks::STRUCTURE }>(start);
        }
        (u32::from(first), pos + 1)
    } else {
        let Some((size, next)) = read_size_fast_cold(in_win, pos) else {
            return if in_win.len() - pos >= SIZE_VARINT_LEN_MAX {
                ElemStep::Fail
            } else {
                ElemStep::NeedGrow(Grow::Mask)
            };
        };
        let next = next.get();
        if !bins.try_record(pos) {
            return ElemStep::NeedGrow(Grow::Bins);
        }
        mask.write_run::<false, { Marks::STRUCTURE }>(start, next);
        (size, next)
    };
    let remaining = input_len - after_len;
    match usize::try_from(size) {
        Ok(len) if len <= remaining => ElemStep::Done(after_len + len),
        _ => ElemStep::Fail,
    }
}

/// Outcome of skipping a run of binary or varint list elements inside a mask window.
#[derive(Clone, Copy)]
enum ElemLoopOutcome {
    Done(usize),
    NeedGrow {
        at: usize,
        remaining: u32,
        grow: Grow,
    },
    Fail,
}

/// Outcome of skipping a list inside a mask window.
#[derive(Clone, Copy)]
enum ListOutcome {
    Done(usize),
    Structs {
        at: usize,
        remaining: u32,
    },
    HeaderNeedGrow,
    TailNeedGrow {
        at: usize,
        code: u8,
        remaining: u32,
        grow: Grow,
    },
    NestedElements {
        at: usize,
        code: u8,
        size: u32,
    },
    Fail,
}

/// Skips `rem` list elements of either binary (`BINARY == true`) or varint (`BINARY == false`)
/// type inside the current window.
#[inline(always)]
fn skip_list_elems_window<const BINARY: bool, M: Marker>(
    in_win: &[u8],
    input_len: usize,
    mut pos: usize,
    mut rem: u32,
    mask: &mut M::Mask<'_>,
    bins: &mut M::Bins<'_>,
) -> ElemLoopOutcome {
    while rem > 0 {
        let step = if BINARY {
            skip_binary_window::<false, _, _>(in_win, input_len, pos, mask, bins)
        } else if let Some(after) = skip_varint_fast::<false, _>(in_win, pos, mask) {
            ElemStep::Done(after)
        } else {
            ElemStep::NeedGrow(Grow::Mask)
        };
        match step {
            ElemStep::Done(after) => {
                pos = after;
                rem -= 1;
            }
            ElemStep::NeedGrow(grow) if M::GROWABLE => {
                return ElemLoopOutcome::NeedGrow {
                    at: pos,
                    remaining: rem,
                    grow,
                };
            }
            ElemStep::NeedGrow(_) | ElemStep::Fail => return ElemLoopOutcome::Fail,
        }
    }
    ElemLoopOutcome::Done(pos)
}

#[inline(always)]
fn mark_list_short_header<const FIELD_HEADER: bool, W: MaskWindow>(mask: &mut W, pos: usize) {
    if FIELD_HEADER {
        mask.write_pair::<{ Marks::STRUCTURE }, { Marks::STRUCTURE }>(pos - 1);
    } else {
        mask.write_byte::<{ Marks::STRUCTURE }>(pos);
    }
}

/// Skips a list or set container at `pos` (`depth`) inside `in_win`. When `FIELD_HEADER`
/// is `true`, `pos - 1` holds the 1-byte struct field header, fused into one `strh` store
/// with the 1-byte list header (and left untouched on `HeaderNeedGrow`).
#[inline(always)]
fn skip_list_window<const FIELD_HEADER: bool, M: Marker>(
    in_win: &[u8],
    input_len: usize,
    pos: usize,
    depth: u32,
    mask: &mut M::Mask<'_>,
    bins: &mut M::Bins<'_>,
) -> ListOutcome {
    if depth >= u32::from(SKIP_DEPTH_MAX) {
        return ListOutcome::Fail;
    }
    let Some(&byte) = in_win.get(pos) else {
        return ListOutcome::HeaderNeedGrow;
    };
    if byte == 0 {
        mark_list_short_header::<FIELD_HEADER, _>(mask, pos);
        return ListOutcome::Done(pos + 1);
    }
    let code = byte & 0x0F;
    if !(wire_type::BOOL_TRUE..=wire_type::UUID).contains(&code) {
        return ListOutcome::Fail;
    }
    let short_size = byte >> 4;
    let (size, p) = if short_size < 0x0F {
        mark_list_short_header::<FIELD_HEADER, _>(mask, pos);
        (u32::from(short_size), pos + 1)
    } else {
        let size_pos = pos + 1;
        let Some((size, next)) = read_size_fast_cold(in_win, size_pos) else {
            return if in_win.len() - size_pos >= SIZE_VARINT_LEN_MAX {
                ListOutcome::Fail
            } else {
                ListOutcome::HeaderNeedGrow
            };
        };
        let next = next.get();
        let header_start = if FIELD_HEADER { pos - 1 } else { pos };
        mask.write_run::<false, { Marks::STRUCTURE }>(header_start, next);
        (size, next)
    };
    let avail_u64 = u64::try_from(input_len - p).unwrap_or(u64::MAX);
    if u64::from(size) > avail_u64 {
        return ListOutcome::Fail;
    }
    // `size <= input_len - p <= usize::MAX`, so `size` fits in `usize`.
    let count = usize::try_from(size).unwrap_or(0);
    match code {
        wire_type::BOOL_TRUE..=wire_type::I8 => ListOutcome::Done(p + count),
        wire_type::I16..=wire_type::I64 => {
            match skip_list_elems_window::<false, M>(in_win, input_len, p, size, mask, bins) {
                ElemLoopOutcome::Done(end) => ListOutcome::Done(end),
                ElemLoopOutcome::NeedGrow {
                    at,
                    remaining,
                    grow,
                } => ListOutcome::TailNeedGrow {
                    at,
                    code,
                    remaining,
                    grow,
                },
                ElemLoopOutcome::Fail => ListOutcome::Fail,
            }
        }
        wire_type::DOUBLE if u64::from(size) * 8 <= avail_u64 => ListOutcome::Done(p + count * 8),
        wire_type::BINARY => {
            match skip_list_elems_window::<true, M>(in_win, input_len, p, size, mask, bins) {
                ElemLoopOutcome::Done(end) => ListOutcome::Done(end),
                ElemLoopOutcome::NeedGrow {
                    at,
                    remaining,
                    grow,
                } => ListOutcome::TailNeedGrow {
                    at,
                    code: wire_type::BINARY,
                    remaining,
                    grow,
                },
                ElemLoopOutcome::Fail => ListOutcome::Fail,
            }
        }
        wire_type::STRUCT if size == 0 => ListOutcome::Done(p),
        wire_type::STRUCT if depth + 1 < u32::from(SKIP_DEPTH_MAX) => ListOutcome::Structs {
            at: p,
            remaining: size - 1,
        },
        wire_type::UUID if u64::from(size) * 16 <= avail_u64 => ListOutcome::Done(p + count * 16),
        wire_type::DOUBLE | wire_type::STRUCT | wire_type::UUID => ListOutcome::Fail,
        _ => ListOutcome::NestedElements { at: p, code, size },
    }
}

/// Advances past a fixed-width scalar of `width` bytes, or returns `None` on EOF.
#[inline(always)]
fn skip_fixed_fast(input_len: usize, pos: usize, width: usize) -> Option<usize> {
    if input_len - pos < width {
        return skip_fail();
    }
    Some(pos + width)
}

/// Iterative struct-traversal state preserved across mask/binaries window refills.
struct StructState {
    stack: [u64; INLINE_STACK],
    top: usize,
    rem: u32,
    depth: u32,
}

impl StructState {
    #[inline(always)]
    const fn new(depth: u32, initial_rem: u32) -> Self {
        Self {
            stack: [0; INLINE_STACK],
            top: 0,
            rem: initial_rem,
            depth,
        }
    }

    #[inline(always)]
    fn pack(rem: u32, depth: u32) -> u64 {
        debug_assert!(u8::try_from(depth).is_ok());
        (u64::from(rem) << 32) | u64::from(depth)
    }

    #[inline(always)]
    fn pop_frame(&self, top: &mut usize) -> (u32, u32) {
        *top -= 1;
        let frame = self.stack[*top & (INLINE_STACK - 1)];
        ((frame >> 32) as u32, (frame & 0xFF) as u32)
    }
}

/// Container/rare field exits handled outside [`skip_struct_window`].
#[derive(Clone, Copy)]
enum ComplexExit {
    DeepStructs {
        at: usize,
        remaining: u32,
        child_depth: u32,
    },
    ListTail {
        at: usize,
        code: u8,
        remaining: u32,
        grow: Grow,
    },
    NestedElements {
        at: usize,
        code: u8,
        size: u32,
    },
    Map(usize),
}

/// Outcome of one pass through [`skip_struct_fields_window`].
#[derive(Clone, Copy)]
enum WindowExit<C = ComplexExit> {
    Done(usize),
    Fail,
    NeedGrow { at: usize, grow: Grow },
    LongDelta(usize),
    Complex(C),
}

/// Policy parameterizing [`skip_struct_fields_window`] for either the leaf-struct fast
/// path ([`FlatLoop`]) or the general struct loop ([`StructCursor`]). Because both loops
/// are monomorphized and inlined separately, leaf structs keep isolated branch-predictor
/// history and avoid stack push/pop overhead.
trait StructLoop<M: Marker> {
    type Complex;

    fn finish_struct(&mut self) -> bool;
    fn complex_field(
        &mut self,
        in_win: &[u8],
        input_len: usize,
        pos: usize,
        code: u8,
        mask: &mut M::Mask<'_>,
        bins: &mut M::Bins<'_>,
    ) -> Result<usize, WindowExit<Self::Complex>>;
}

/// Shared struct-field hot loop for both [`FlatLoop`] and [`StructCursor`]. Handles stop
/// markers, short-delta varints, bools, and binaries inline, delegating rare/container
/// fields to `loop_state`.
#[inline(always)]
fn skip_struct_fields_window<L: StructLoop<M>, M: Marker>(
    in_win: &[u8],
    input_len: usize,
    mut pos: usize,
    loop_state: &mut L,
    mask: &mut M::Mask<'_>,
    bins: &mut M::Bins<'_>,
) -> WindowExit<L::Complex> {
    loop {
        let Some(&byte) = in_win.get(pos) else {
            return if M::GROWABLE {
                WindowExit::NeedGrow {
                    at: pos,
                    grow: Grow::Mask,
                }
            } else {
                WindowExit::Fail
            };
        };
        let code = byte & 0x0F;
        if code == wire_type::STOP {
            mask.write_byte::<{ Marks::STRUCTURE }>(pos);
            pos += 1;
            if loop_state.finish_struct() {
                return WindowExit::Done(pos);
            }
            continue;
        }
        if byte >> 4 == 0 {
            return WindowExit::LongDelta(pos);
        }
        if (wire_type::I16..=wire_type::I64).contains(&code) {
            if let Some(after) = skip_varint_fast::<true, _>(in_win, pos, mask) {
                pos = after;
                continue;
            }
            return if M::GROWABLE {
                WindowExit::NeedGrow {
                    at: pos,
                    grow: Grow::Mask,
                }
            } else {
                WindowExit::Fail
            };
        }
        if code <= wire_type::BOOL_FALSE {
            mask.write_byte::<{ Marks::STRUCTURE }>(pos);
            pos += 1;
            continue;
        }
        if code == wire_type::BINARY {
            match skip_binary_window::<true, _, _>(in_win, input_len, pos, mask, bins) {
                ElemStep::Done(after) => {
                    pos = after;
                    continue;
                }
                ElemStep::NeedGrow(grow) if M::GROWABLE => {
                    return WindowExit::NeedGrow { at: pos, grow };
                }
                ElemStep::NeedGrow(_) | ElemStep::Fail => return WindowExit::Fail,
            }
        }
        match loop_state.complex_field(in_win, input_len, pos, code, mask, bins) {
            Ok(after) => pos = after,
            Err(exit) => return exit,
        }
    }
}

/// Leaf-struct loop state (`PageLocation`, `Statistics`, `KeyValue`, `SchemaElement`,
/// `PageEncodingStats`): handles short-delta `Bool`, integer, and `Binary` fields and
/// bails out on any other field.
struct FlatLoop {
    remaining: u32,
}

impl<M: Marker> StructLoop<M> for FlatLoop {
    type Complex = usize;

    #[inline(always)]
    fn finish_struct(&mut self) -> bool {
        if self.remaining == 0 {
            return true;
        }
        self.remaining -= 1;
        false
    }

    #[inline(always)]
    fn complex_field(
        &mut self,
        _in_win: &[u8],
        _input_len: usize,
        pos: usize,
        _code: u8,
        _mask: &mut M::Mask<'_>,
        _bins: &mut M::Bins<'_>,
    ) -> Result<usize, WindowExit<usize>> {
        Err(WindowExit::Complex(pos))
    }
}

/// Active iterative-traversal cursor kept in locals inside [`skip_struct_window`]
/// (zero-cost when `ITERATIVE == false`).
struct StructCursor<'s, const ITERATIVE: bool> {
    state: &'s mut StructState,
    top: usize,
    rem: u32,
    depth: u32,
}

impl<const ITERATIVE: bool> StructCursor<'_, ITERATIVE> {
    #[inline(always)]
    fn push_child(&mut self, child_rem: u32, child_depth: u32) -> bool {
        if self.top < INLINE_STACK {
            self.state.stack[self.top] = StructState::pack(self.rem, self.depth);
            self.top += 1;
            self.rem = child_rem;
            self.depth = child_depth;
            true
        } else {
            false
        }
    }
}

impl<const ITERATIVE: bool, M: Marker> StructLoop<M> for StructCursor<'_, ITERATIVE> {
    type Complex = ComplexExit;

    #[inline(always)]
    fn finish_struct(&mut self) -> bool {
        if !ITERATIVE {
            return true;
        }
        if self.rem != 0 {
            self.rem -= 1;
            return false;
        }
        if self.top == 0 {
            return true;
        }
        (self.rem, self.depth) = self.state.pop_frame(&mut self.top);
        false
    }

    #[inline(always)]
    fn complex_field(
        &mut self,
        in_win: &[u8],
        input_len: usize,
        pos: usize,
        code: u8,
        mask: &mut M::Mask<'_>,
        bins: &mut M::Bins<'_>,
    ) -> Result<usize, WindowExit> {
        skip_complex_field_window::<ITERATIVE, M>(in_win, input_len, pos, code, self, mask, bins)
    }
}

/// Resolves a flat leaf-struct exit, pushing a child frame onto the inline stack when
/// `ITERATIVE` and capacity permit.
#[inline(always)]
fn resolve_flat_outcome<const ITERATIVE: bool>(
    outcome: WindowExit<usize>,
    remaining: u32,
    child_depth: u32,
    cur: &mut StructCursor<'_, ITERATIVE>,
) -> Result<usize, WindowExit> {
    match outcome {
        WindowExit::Done(after) => Ok(after),
        WindowExit::LongDelta(at) | WindowExit::Complex(at) => {
            if ITERATIVE && cur.push_child(remaining, child_depth) {
                Ok(at)
            } else {
                Err(WindowExit::Complex(ComplexExit::DeepStructs {
                    at,
                    remaining,
                    child_depth,
                }))
            }
        }
        WindowExit::NeedGrow { at, grow } => {
            if ITERATIVE && cur.push_child(remaining, child_depth) {
                Err(WindowExit::NeedGrow { at, grow })
            } else {
                Err(WindowExit::Complex(ComplexExit::DeepStructs {
                    at,
                    remaining,
                    child_depth,
                }))
            }
        }
        WindowExit::Fail => Err(WindowExit::Fail),
    }
}

/// Dispatches non-scalar/rare short-delta fields (`I8`, `Double`, `List`, `Set`, `Map`,
/// `Struct`, `Uuid`, or invalid) and runs child structs through [`FlatLoop`] without an
/// intermediate enum.
#[inline(always)]
fn skip_complex_field_window<const ITERATIVE: bool, M: Marker>(
    in_win: &[u8],
    input_len: usize,
    pos: usize,
    code: u8,
    cur: &mut StructCursor<'_, ITERATIVE>,
    mask: &mut M::Mask<'_>,
    bins: &mut M::Bins<'_>,
) -> Result<usize, WindowExit> {
    let next = pos + 1;
    let (child_at, child_rem, child_depth) = match code {
        wire_type::I8 | wire_type::DOUBLE | wire_type::UUID => {
            let width = match code {
                wire_type::I8 => 1,
                wire_type::DOUBLE => 8,
                _ => 16,
            };
            let Some(after) = skip_fixed_fast(input_len, next, width) else {
                return Err(WindowExit::Fail);
            };
            mask.write_byte::<{ Marks::STRUCTURE }>(pos);
            return Ok(after);
        }
        wire_type::LIST | wire_type::SET => {
            match skip_list_window::<true, M>(in_win, input_len, next, cur.depth + 1, mask, bins) {
                ListOutcome::Done(after) => return Ok(after),
                ListOutcome::Structs { at, remaining } => (at, remaining, cur.depth + 2),
                ListOutcome::HeaderNeedGrow if M::GROWABLE => {
                    return Err(WindowExit::NeedGrow {
                        at: pos,
                        grow: Grow::Mask,
                    });
                }
                ListOutcome::TailNeedGrow {
                    at,
                    code: elem_code,
                    remaining,
                    grow,
                } if M::GROWABLE => {
                    return Err(WindowExit::Complex(ComplexExit::ListTail {
                        at,
                        code: elem_code,
                        remaining,
                        grow,
                    }));
                }
                ListOutcome::NestedElements {
                    at,
                    code: elem_code,
                    size,
                } => {
                    return Err(WindowExit::Complex(ComplexExit::NestedElements {
                        at,
                        code: elem_code,
                        size,
                    }));
                }
                ListOutcome::HeaderNeedGrow
                | ListOutcome::TailNeedGrow { .. }
                | ListOutcome::Fail => {
                    return Err(WindowExit::Fail);
                }
            }
        }
        wire_type::MAP => {
            mask.write_byte::<{ Marks::STRUCTURE }>(pos);
            return Err(WindowExit::Complex(ComplexExit::Map(next)));
        }
        wire_type::STRUCT => {
            let child_depth = cur.depth + 1;
            if child_depth >= u32::from(SKIP_DEPTH_MAX) {
                return Err(WindowExit::Fail);
            }
            mask.write_byte::<{ Marks::STRUCTURE }>(pos);
            (next, 0, child_depth)
        }
        _ => return Err(WindowExit::Fail),
    };
    let mut flat = FlatLoop {
        remaining: child_rem,
    };
    let outcome =
        skip_struct_fields_window::<_, M>(in_win, input_len, child_at, &mut flat, mask, bins);
    resolve_flat_outcome::<ITERATIVE>(outcome, flat.remaining, child_depth, cur)
}

/// Inner hot loop of [`skip_struct_fast`], running flat leaf structs inline via
/// [`FlatLoop`]. When `ITERATIVE` is `false` (top-level entry), no inline stack is
/// allocated or zeroed and `code == 0` returns in one branch; encountering a non-flat
/// child struct transitions once to `ITERATIVE = true`.
#[inline(always)]
fn skip_struct_window<const ITERATIVE: bool, M: Marker>(
    in_win: &[u8],
    input_len: usize,
    pos: usize,
    state: &mut StructState,
    mask: &mut M::Mask<'_>,
    bins: &mut M::Bins<'_>,
) -> WindowExit {
    let mut cur = StructCursor::<ITERATIVE> {
        top: if ITERATIVE { state.top } else { 0 },
        rem: if ITERATIVE { state.rem } else { 0 },
        depth: state.depth,
        state,
    };
    let exit = skip_struct_fields_window::<_, M>(in_win, input_len, pos, &mut cur, mask, bins);
    if ITERATIVE && !matches!(exit, WindowExit::Done(_)) {
        cur.state.top = cur.top;
        cur.state.rem = cur.rem;
        cur.state.depth = cur.depth;
    }
    exit
}

/// Skips a top-level or nested list/set container using the fast register-return path,
/// handing `List<Struct>` directly to [`skip_struct_fast`].
fn skip_list_top_fast<M: Marker>(
    input: &[u8],
    pos: usize,
    depth: u32,
    marker: &mut M,
) -> Option<usize> {
    if depth >= u32::from(SKIP_DEPTH_MAX) {
        return None;
    }
    loop {
        if pos >= input.len() {
            return skip_fail();
        }
        let (limit, mut mask, mut bins) = marker.window(pos, input.len());
        let orig_bins = bins.remaining();
        let in_win = &input[..limit];
        let outcome =
            skip_list_window::<false, M>(in_win, input.len(), pos, depth, &mut mask, &mut bins);
        let written = orig_bins - bins.remaining();
        drop((mask, bins));
        marker.commit_bins(written);
        match outcome {
            ListOutcome::Done(after) => return Some(after),
            ListOutcome::Structs { at, remaining } => {
                return skip_struct_fast::<true, _>(input, at, depth + 1, remaining, marker);
            }
            ListOutcome::HeaderNeedGrow if M::GROWABLE && limit < input.len() => {
                if !marker.try_grow_mask(pos + MASK_GROW_LOOKAHEAD, input.len()) {
                    return None;
                }
            }
            ListOutcome::TailNeedGrow {
                at,
                code,
                remaining,
                grow,
            } if M::GROWABLE && (matches!(grow, Grow::Bins) || limit < input.len()) => {
                return skip_list_tail_growing(input, at, code, remaining, grow, marker);
            }
            ListOutcome::NestedElements { at, code, size } => {
                return skip_nested_elements_fast_cold(input, at, code, size, depth + 1, marker);
            }
            ListOutcome::HeaderNeedGrow | ListOutcome::TailNeedGrow { .. } | ListOutcome::Fail => {
                return skip_fail();
            }
        }
    }
}

/// Top-level single-struct skip entry. Unmarked skips have ample registers to run
/// iteratively from the top (cutting a monomorphization and speeding up page-index and
/// medium/wide skips); marking skips saturate GPRs with mask/bins pointers, so keeping a
/// flat top-level entry avoids spilling the inline stack on small flat structs.
#[inline(always)]
pub(super) fn skip_struct_top_fast<M: Marker>(
    input: &[u8],
    pos: usize,
    depth: u32,
    marker: &mut M,
) -> Option<usize> {
    if M::GROWABLE {
        skip_struct_fast::<false, _>(input, pos, depth, 0, marker)
    } else {
        skip_struct_fast::<true, _>(input, pos, depth, 0, marker)
    }
}

/// Skips `initial_rem + 1` consecutive structs starting at `pos` (`depth`), returning
/// `Some(new_pos)` in registers or `None` on any malformed input. Keeps error construction
/// and sret stack frames completely out of the hot path.
fn skip_struct_fast<const ITERATIVE: bool, M: Marker>(
    input: &[u8],
    mut pos: usize,
    depth: u32,
    initial_rem: u32,
    marker: &mut M,
) -> Option<usize> {
    debug_assert!(depth < u32::from(SKIP_DEPTH_MAX));
    let mut state = StructState::new(depth, initial_rem);
    loop {
        if pos >= input.len() {
            return skip_fail();
        }
        let (limit, mut mask, mut bins) = marker.window(pos, input.len());
        let orig_bins = bins.remaining();
        let in_win = &input[..limit];
        let exit = skip_struct_window::<ITERATIVE, M>(
            in_win,
            input.len(),
            pos,
            &mut state,
            &mut mask,
            &mut bins,
        );
        let written = orig_bins - bins.remaining();
        drop((mask, bins));
        marker.commit_bins(written);
        match exit {
            WindowExit::Done(end) => return Some(end),
            WindowExit::Fail => return skip_fail(),
            WindowExit::NeedGrow {
                at: resume_pos,
                grow: Grow::Mask,
            } => {
                if !marker.try_grow_mask(resume_pos + MASK_GROW_LOOKAHEAD, input.len()) {
                    return skip_fail();
                }
                pos = resume_pos;
            }
            WindowExit::NeedGrow {
                at: resume_pos,
                grow: Grow::Bins,
            } => {
                if marker.can_use_bins_window(input.len()) {
                    marker.grow_bins();
                    pos = resume_pos;
                } else {
                    pos = skip_field_binary_cold(input, resume_pos, marker)?;
                }
            }
            WindowExit::LongDelta(at) => {
                pos = skip_long_delta_field_cold(input, at, state.depth, marker)?;
            }
            WindowExit::Complex(ComplexExit::DeepStructs {
                at,
                remaining,
                child_depth,
            }) => {
                pos = skip_struct_fast::<true, _>(input, at, child_depth, remaining, marker)?;
            }
            WindowExit::Complex(ComplexExit::ListTail {
                at,
                code,
                remaining,
                grow,
            }) => {
                pos = skip_list_tail_growing(input, at, code, remaining, grow, marker)?;
            }
            WindowExit::Complex(ComplexExit::NestedElements { at, code, size }) => {
                pos =
                    skip_nested_elements_fast_cold(input, at, code, size, state.depth + 2, marker)?;
            }
            WindowExit::Complex(ComplexExit::Map(at)) => {
                pos = skip_map_fast_cold(input, at, state.depth + 1, marker)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thrift::VarintTarget;

    #[test]
    fn skip_marking_does_not_grow_mask_on_malformed_sizes() {
        let large_tail = 1 << 20;
        // 1. Binary field with overflowing u32 varint size.
        let mut binary_overflow = vec![0x18, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        binary_overflow.resize(large_tail, 0);
        // 2. Binary field with size > i32::MAX (0x8000_0000).
        let mut binary_size_max = vec![0x18, 0x80, 0x80, 0x80, 0x80, 0x08];
        binary_size_max.resize(large_tail, 0);
        // 3. Struct field holding a long-size list whose size varint overflows u32.
        let mut list_field_overflow = vec![0x19, 0xF5, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        list_field_overflow.resize(large_tail, 0);
        // 4. Top-level long-size list whose size > i32::MAX.
        let mut top_list_size_max = vec![0xF5, 0x80, 0x80, 0x80, 0x80, 0x08];
        top_list_size_max.resize(large_tail, 0);
        // 5. Binary element inside a list with overflowing size varint.
        let mut list_binary_overflow = vec![0x19, 0x18, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        list_binary_overflow.resize(large_tail, 0);

        let cases = [
            (
                ElementType::Struct,
                &binary_overflow,
                ThriftError::VarintOverflow {
                    offset: 1,
                    target: VarintTarget::U32,
                },
            ),
            (
                ElementType::Struct,
                &binary_size_max,
                ThriftError::SizeOverflow {
                    offset: 1,
                    size: 0x8000_0000,
                },
            ),
            (
                ElementType::Struct,
                &list_field_overflow,
                ThriftError::VarintOverflow {
                    offset: 2,
                    target: VarintTarget::U32,
                },
            ),
            (
                ElementType::List,
                &top_list_size_max,
                ThriftError::SizeOverflow {
                    offset: 0,
                    size: 0x8000_0000,
                },
            ),
            (
                ElementType::Struct,
                &list_binary_overflow,
                ThriftError::VarintOverflow {
                    offset: 2,
                    target: VarintTarget::U32,
                },
            ),
        ];

        for (element_type, input, expected) in cases {
            let mut plain = CompactReader::new(input, 0);
            assert_eq!(plain.skip(element_type), Err(expected));

            let mut marks = Marks::new();
            let mut marking = CompactReader::new(input, 0);
            assert_eq!(
                marking.skip_marking(element_type, &mut marks),
                Err(expected)
            );
            assert_eq!(marks.mask(), []);
            // Initial window is ~4 KiB; malformed size must fail without growing toward 1 MiB.
            assert!(
                marks.mask_backing_len() <= 8192,
                "mask grew to {} bytes on malformed size",
                marks.mask_backing_len()
            );
        }
    }
}
