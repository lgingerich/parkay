//! Structural bit masks and binary offset tables recorded by
//! [`CompactReader::skip_marking`](super::CompactReader::skip_marking), plus the
//! windowed marker abstractions used by the skip engine.

/// The fewest zeros a mask is extended by when marking reaches its end. Marks are
/// written a word at a time, so it must be at least a word.
const MASK_GROWTH_SIZE_MIN: usize = 4096;

/// The fewest binary-offset slots added when `Marks::binaries` grows (512 bytes), so
/// small footers do not allocate and zero 16 KiB on their first binary.
const BINARIES_GROWTH_COUNT_MIN: usize = 128;

const _: () = assert!(MASK_GROWTH_SIZE_MIN >= size_of::<u64>());
const _: () = assert!(BINARIES_GROWTH_COUNT_MIN > 0);

/// Which bits of skipped values decided where the values end, as
/// [`CompactReader::skip_marking`](super::CompactReader::skip_marking) records them.
///
/// Skipping reads some bits of its input and not others. It reads every bit of a field
/// header or of a size, only the continuation bit of each byte of an integer, and
/// nothing of a binary's contents. So bytes that agree with a skipped value on every
/// bit that was read are skipped by the same steps, and hold a value of the same
/// length. A caller can pass over them by comparing instead of parsing.
///
/// Two things can differ further without changing the steps: an integer can have any
/// length, and so can a binary. The mask shows where the integers are and the binaries
/// are listed, so a caller that finds a difference in one of them can step over it in
/// both values and go on comparing.
#[derive(Debug, Default)]
pub struct Marks {
    /// The mask, and after it zeros for marks to come to be written into.
    mask: Vec<u8>,
    /// How much of `mask` is marks.
    mask_size: usize,
    /// Binary offsets, and after them zeros for offsets to come to be written into.
    binaries: Vec<u32>,
    /// How much of `binaries` is recorded offsets.
    binaries_size: usize,
}

impl Marks {
    /// In the mask: every bit of the byte was read. Field headers, stop markers, list
    /// and map headers, and the sizes of lists and binaries are marked this way.
    pub const STRUCTURE: u8 = 0xFF;
    /// In the mask: only the continuation bit was read. This is a byte of an integer's
    /// varint, whose other seven bits are part of the integer.
    pub const VARINT: u8 = 0x80;
    /// In the mask: no bit was read. This is a byte of a binary's contents, or of a
    /// value of fixed width.
    pub const CONTENT: u8 = 0x00;

    /// Marks of nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mask: Vec::new(),
            mask_size: 0,
            binaries: Vec::new(),
            binaries_size: 0,
        }
    }

    /// Forgets every mark, and keeps the memory for the next ones.
    #[cfg_attr(
        not(feature = "internals"),
        expect(
            dead_code,
            reason = "used by the benchmarks, which mark one footer many times"
        )
    )]
    pub fn clear(&mut self) {
        self.mask.clear();
        self.mask_size = 0;
        self.binaries.clear();
        self.binaries_size = 0;
    }

    /// One byte for each byte skipped, in order, with the bits set that were read.
    #[must_use]
    pub const fn mask(&self) -> &[u8] {
        self.mask.as_slice().split_at(self.mask_size).0
    }

    /// For each binary skipped, in order, the index in [`mask`](Self::mask) of the
    /// first byte of its length.
    ///
    /// A binary past the first 4 GiB of mask is not listed. That only keeps a caller
    /// from stepping over it.
    #[must_use]
    pub const fn binaries(&self) -> &[u32] {
        self.binaries.as_slice().split_at(self.binaries_size).0
    }

    /// Current backing length of the mask buffer, including zeroed slack.
    #[cfg(test)]
    pub(super) const fn mask_backing_len(&self) -> usize {
        self.mask.len()
    }

    /// Snapshots the current lengths before a `skip_marking` call so failures can roll back.
    pub(super) const fn snapshot(&self) -> (usize, usize) {
        (self.mask_size, self.binaries_size)
    }

    /// Cold tail extension when a skipped value ends in unmarked contents past the current
    /// mask buffer length.
    #[cold]
    #[inline(never)]
    fn resize_mask_tail(&mut self, mask_end: usize) {
        self.mask.resize(mask_end, Self::CONTENT);
    }

    /// Commits `consumed` bytes of marks after a successful `skip_marking` call.
    pub(super) fn finish_skip(&mut self, mask_start: usize, consumed: usize) {
        // One mark for each byte passed over. The value may end with contents, which have
        // no marks written, beyond the zeros the mask already has.
        let mask_end = mask_start + consumed;
        if self.mask.len() < mask_end {
            self.resize_mask_tail(mask_end);
        } else {
            // The mask already reaches that far.
        }
        self.mask_size = mask_end;
    }

    /// Restores the zero-filled tail and binary count after a failed `skip_marking` call.
    pub(super) fn rollback_skip(&mut self, mask_start: usize, binaries_start: usize) {
        debug_assert!(mask_start <= self.mask.len());
        debug_assert!(binaries_start <= self.binaries.len());
        // Everything past the marks that were there is zeros again.
        self.mask[mask_start..].fill(Self::CONTENT);
        self.binaries_size = binaries_start;
    }
}

/// Builds 8-byte mask words for 2..=5-byte SWAR varints, with or without a preceding
/// 1-byte field header. Indexed by `(term.trailing_zeros() >> 3)` (`0..=3`).
const fn swar_varint_words(with_header: bool) -> [[u8; 8]; 4] {
    let mut table = [[Marks::CONTENT; 8]; 4];
    let mut idx = 0;
    while idx < 4 {
        let mut i = if with_header {
            table[idx][0] = Marks::STRUCTURE;
            1
        } else {
            0
        };
        let end = i + 2 + idx;
        while i < end {
            table[idx][i] = Marks::VARINT;
            i += 1;
        }
        idx += 1;
    }
    table
}

const VARINT_WORDS: [[u8; 8]; 4] = swar_varint_words(false);
const FIELD_VARINT_WORDS: [[u8; 8]; 4] = swar_varint_words(true);

/// Borrowed window into the output mask covering `input[..limit]` plus 8 trailing zeroed
/// slack bytes, so every `pos < limit` satisfies `pos + 8 <= mask.len()`.
///
/// For plain `skip` this is `()` (zero-sized, compiles to nothing). For `skip_marking`
/// this is `&mut [u8]`, giving LLVM a direct `noalias` register pointer whose length is
/// tied to `in_win.len() + 8` so per-field mask bounds checks and struct reloads vanish.
pub(super) trait MaskWindow {
    fn write_byte<const BYTE: u8>(&mut self, pos: usize);
    fn write_pair<const FIRST: u8, const SECOND: u8>(&mut self, pos: usize);
    fn write_swar<const WITH_HEADER: bool>(&mut self, pos: usize, idx: usize);
    fn write_run<const WITH_HEADER: bool, const BYTE: u8>(&mut self, start: usize, end: usize);
}

impl MaskWindow for () {
    fn write_byte<const BYTE: u8>(&mut self, _pos: usize) {}
    fn write_pair<const FIRST: u8, const SECOND: u8>(&mut self, _pos: usize) {}
    fn write_swar<const WITH_HEADER: bool>(&mut self, _pos: usize, _idx: usize) {}
    fn write_run<const WITH_HEADER: bool, const BYTE: u8>(&mut self, _start: usize, _end: usize) {}
}

/// Cold fill when a varint, header + varint, or size spans more than 8 bytes.
/// `fill` packs the first byte in the low 8 bits and the remaining bytes in the high 8.
#[cold]
#[inline(never)]
fn fill_mask_long(mask: &mut [u8], start: usize, end: usize, fill: u16) {
    let [first_byte, rest_byte] = fill.to_le_bytes();
    if let Some(slice) = mask.get_mut(start..end) {
        slice.fill(rest_byte);
        if let Some(first) = slice.first_mut() {
            *first = first_byte;
        }
    }
}

impl MaskWindow for &mut [u8] {
    #[inline(always)]
    fn write_byte<const BYTE: u8>(&mut self, pos: usize) {
        if let Some(slot) = self.get_mut(pos) {
            *slot = BYTE;
        }
    }

    #[inline(always)]
    fn write_pair<const FIRST: u8, const SECOND: u8>(&mut self, pos: usize) {
        if let Some(pair) = self
            .get_mut(pos..)
            .and_then(|rest| rest.first_chunk_mut::<2>())
        {
            *pair = [FIRST, SECOND];
        }
    }

    #[inline(always)]
    fn write_swar<const WITH_HEADER: bool>(&mut self, pos: usize, idx: usize) {
        debug_assert!(idx < 4);
        if let Some(word) = self
            .get_mut(pos..)
            .and_then(|rest| rest.first_chunk_mut::<8>())
        {
            debug_assert_eq!(*word, [Marks::CONTENT; 8]);
            let table = if WITH_HEADER {
                &FIELD_VARINT_WORDS
            } else {
                &VARINT_WORDS
            };
            *word = table[idx & 3];
        }
    }

    #[inline(always)]
    fn write_run<const WITH_HEADER: bool, const BYTE: u8>(&mut self, start: usize, end: usize) {
        debug_assert!(start < end);
        debug_assert!(!WITH_HEADER || end >= start + 2);
        let count = end - start;
        if count <= 8 {
            if let Some(word) = self
                .get_mut(start..)
                .and_then(|rest| rest.first_chunk_mut::<8>())
            {
                debug_assert_eq!(*word, [Marks::CONTENT; 8]);
                let shift = (8 - count) * 8;
                let fill = u64::from_le_bytes([BYTE; 8]) >> shift;
                let bits = if WITH_HEADER {
                    fill | u64::from(Marks::STRUCTURE)
                } else {
                    fill
                };
                *word = bits.to_le_bytes();
            }
        } else {
            let first = if WITH_HEADER { Marks::STRUCTURE } else { BYTE };
            fill_mask_long(self, start, end, u16::from_le_bytes([first, BYTE]));
        }
    }
}

/// Borrowed binary-offset window split from the mask buffer so recording a binary offset
/// uses register-resident slice state rather than reloading `Vec<u32>` fields from memory.
pub(super) trait BinariesWindow {
    /// Remaining free slots in this window (used to compute written count on exit).
    fn remaining(&self) -> usize;

    /// Records a binary length offset at `pos`, returning `false` when the window is full
    /// (or disabled past 4 GiB) so the caller can grow or fall back cold.
    fn try_record(&mut self, pos: usize) -> bool;
}

impl BinariesWindow for () {
    #[inline(always)]
    fn remaining(&self) -> usize {
        0
    }

    #[inline(always)]
    fn try_record(&mut self, _pos: usize) -> bool {
        true
    }
}

/// Shrinking slice window into `Marks::binaries`. Holding only `(buf, base)` avoids
/// spilling a separate `written` counter inside the hot loop; written count is recovered
/// on window exit as `orig_len - buf.len()`.
pub(super) struct BinariesSlice<'b> {
    buf: &'b mut [u32],
    base: u32,
}

impl BinariesWindow for BinariesSlice<'_> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.buf.len()
    }

    #[inline(always)]
    fn try_record(&mut self, pos: usize) -> bool {
        let buf = std::mem::take(&mut self.buf);
        let Some((slot, rest)) = buf.split_first_mut() else {
            return false;
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "non-empty window guarantees base + pos <= u32::MAX"
        )]
        {
            *slot = self.base.wrapping_add(pos as u32);
        }
        self.buf = rest;
        true
    }
}

/// Cold empty binary-offset window returned only when `mask_start + input_len > u32::MAX`.
/// Kept out of line so LLVM keeps the `<= 4 GiB` path as straight-line fallthrough and
/// does not unswitch `skip_list_top_fast::<Marking>` on this check.
#[cold]
#[inline(never)]
fn bins_overflow_window() -> &'static mut [u32] {
    &mut []
}

/// What a skip tells of the bytes it reads. It tells of them in the order it reads
/// them. A plain skip tells nothing, and compiles to the same code as if this were not
/// here.
pub(super) trait Marker {
    type Mask<'m>: MaskWindow
    where
        Self: 'm;
    type Bins<'m>: BinariesWindow
    where
        Self: 'm;

    /// `false` for [`Unmarked`] so window-growth paths fold away at compile time.
    const GROWABLE: bool;

    /// Borrows a window covering `input[..limit]` (with 8 trailing slack bytes in the
    /// mask), growing the mask first if `pos` is already past the current buffer.
    fn window(&mut self, pos: usize, input_len: usize) -> (usize, Self::Mask<'_>, Self::Bins<'_>);

    /// Commits `written` binary offsets recorded into the last window.
    fn commit_bins(&mut self, written: usize);

    /// Whether `mask_start + input_len <= u32::MAX` (so `BinariesSlice` is active).
    fn can_use_bins_window(&self, input_len: usize) -> bool;

    /// Grows the backing mask if it does not already cover `input_len + 8` bytes past
    /// `mask_start`, returning `false` if the window already reached the end of `input`.
    fn try_grow_mask(&mut self, needed: usize, input_len: usize) -> bool;

    /// Grows the backing `binaries` buffer by at least 1.5x (minimum
    /// `BINARIES_GROWTH_COUNT_MIN`).
    fn grow_bins(&mut self);

    /// Cold path: marks `input[start..end]` with `byte` (`skip_map`, `skip_nested`, etc.).
    fn mark_cold(&mut self, start: usize, end: usize, byte: u8);

    /// Cold path: records a binary length at `start` and marks `input[start..end]`.
    fn mark_binary_cold(&mut self, start: usize, end: usize);
}

/// The marker of a plain skip.
pub(super) struct Unmarked;

impl Marker for Unmarked {
    type Mask<'m> = ();
    type Bins<'m> = ();
    const GROWABLE: bool = false;

    #[inline(always)]
    fn window(&mut self, _pos: usize, input_len: usize) -> (usize, (), ()) {
        (input_len, (), ())
    }

    #[inline(always)]
    fn commit_bins(&mut self, _written: usize) {}

    #[inline(always)]
    fn can_use_bins_window(&self, _input_len: usize) -> bool {
        true
    }

    #[inline(always)]
    fn try_grow_mask(&mut self, _needed: usize, _input_len: usize) -> bool {
        false
    }

    #[inline(always)]
    fn grow_bins(&mut self) {}

    fn mark_cold(&mut self, _start: usize, _end: usize, _byte: u8) {}
    fn mark_binary_cold(&mut self, _start: usize, _end: usize) {}
}

/// The marker of [`CompactReader::skip_marking`](super::CompactReader::skip_marking).
///
/// Borrows `Marks` directly for the duration of one `skip_marking` call so small-struct
/// callers avoid moving two `Vec`s in and out. The input slice starts at `input_start`,
/// so input position `pos` maps directly to `mask[mask_start + pos]`.
pub(super) struct Marking<'a> {
    marks: &'a mut Marks,
    mask_start: usize,
}

/// Computes the next geometric buffer length: at least 1.5x `cur` (minimum `+min_growth`)
/// and at least `needed + min_growth`.
fn grown_len(cur: usize, needed: usize, min_growth: usize) -> usize {
    (needed + min_growth).max(cur + (cur / 2).max(min_growth))
}

impl<'a> Marking<'a> {
    pub(super) const fn new(marks: &'a mut Marks, mask_start: usize) -> Self {
        Self { marks, mask_start }
    }

    /// Extends `marks.mask` by at least 1.5x (minimum `MASK_GROWTH_SIZE_MIN`), capped at
    /// `mask_start + input_len + 8` so large footers never zero unreachable memory past
    /// EOF while small chunk marks still grow gently.
    #[cold]
    #[inline(never)]
    fn grow_to(&mut self, needed: usize, input_len: usize) -> bool {
        let max_len = self.mask_start + input_len + size_of::<u64>();
        let cur = self.marks.mask.len();
        if cur >= max_len {
            return false;
        }
        let grown = grown_len(cur, needed, MASK_GROWTH_SIZE_MIN)
            .min(max_len)
            .max(needed);
        self.marks.mask.resize(grown, Marks::CONTENT);
        true
    }

    /// Extends `marks.binaries` by at least 1.5x (minimum `BINARIES_GROWTH_COUNT_MIN`).
    #[cold]
    #[inline(never)]
    fn grow_binaries(&mut self) {
        let grown = grown_len(
            self.marks.binaries.len(),
            self.marks.binaries_size,
            BINARIES_GROWTH_COUNT_MIN,
        );
        self.marks.binaries.resize(grown, 0);
    }

    /// Cold binary-offset push used outside the fast window (`skip_map`, `skip_nested`,
    /// >4 GiB fallback).
    fn push_binary_cold(&mut self, index: usize) {
        if let Ok(bin_index) = u32::try_from(index) {
            if self.marks.binaries_size == self.marks.binaries.len() {
                self.grow_binaries();
            }
            self.marks.binaries[self.marks.binaries_size] = bin_index;
            self.marks.binaries_size += 1;
        } else {
            // Not listed: see `Marks::binaries`.
        }
    }

    /// Cold fallback for scalar `skip_nested` / `skip_map` marking.
    #[cold]
    #[inline(never)]
    fn set_slowly(&mut self, index: usize, count: usize, byte: u8) {
        let needed = index + count.max(size_of::<u64>());
        if self.marks.mask.len() < needed {
            let grown = grown_len(self.marks.mask.len(), needed, MASK_GROWTH_SIZE_MIN);
            self.marks.mask.resize(grown, Marks::CONTENT);
        }
        self.marks.mask[index..index + count].fill(byte);
    }
}

impl Marker for Marking<'_> {
    type Mask<'m>
        = &'m mut [u8]
    where
        Self: 'm;
    type Bins<'m>
        = BinariesSlice<'m>
    where
        Self: 'm;
    const GROWABLE: bool = true;

    #[inline(always)]
    fn window(&mut self, pos: usize, input_len: usize) -> (usize, &mut [u8], BinariesSlice<'_>) {
        let needed = self.mask_start + pos + size_of::<u64>();
        if self.marks.mask.len() < needed {
            self.grow_to(needed, input_len);
        }
        let marks = &mut *self.marks;
        let tail = &mut marks.mask[self.mask_start..];
        let limit = input_len.min(tail.len() - size_of::<u64>());
        let mask = &mut tail[..limit + size_of::<u64>()];
        let buf = if u32::try_from(self.mask_start.saturating_add(input_len)).is_ok() {
            &mut marks.binaries[marks.binaries_size..]
        } else {
            bins_overflow_window()
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "base is only read when buf is non-empty (mask_start + input_len <= u32::MAX)"
        )]
        let bins = BinariesSlice {
            buf,
            base: self.mask_start as u32,
        };
        (limit, mask, bins)
    }

    #[inline(always)]
    fn commit_bins(&mut self, written: usize) {
        self.marks.binaries_size += written;
    }

    #[inline(always)]
    fn can_use_bins_window(&self, input_len: usize) -> bool {
        u32::try_from(self.mask_start.saturating_add(input_len)).is_ok()
    }

    #[inline(always)]
    fn try_grow_mask(&mut self, needed: usize, input_len: usize) -> bool {
        self.grow_to(self.mask_start + needed, input_len)
    }

    #[inline(always)]
    fn grow_bins(&mut self) {
        self.grow_binaries();
    }

    fn mark_cold(&mut self, start: usize, end: usize, byte: u8) {
        self.set_slowly(self.mask_start + start, end - start, byte);
    }

    #[cold]
    #[inline(never)]
    fn mark_binary_cold(&mut self, start: usize, end: usize) {
        let index = self.mask_start + start;
        self.push_binary_cold(index);
        self.set_slowly(index, end - start, Marks::STRUCTURE);
    }
}
