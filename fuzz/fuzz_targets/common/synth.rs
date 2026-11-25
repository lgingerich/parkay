//! Zero-dependency byte-driven generator and Thrift compact-protocol encoder for
//! structure-aware fuzz targets.
//!
//! Consumes fuzz bytes sequentially (falling back to 0 when exhausted) so libFuzzer's
//! mutations map directly to structural choices rather than failing on the first invalid
//! wire byte.

#![expect(
    dead_code,
    reason = "each structured fuzz target uses a subset of the encoder helpers"
)]

/// Cursor over raw fuzz input bytes for deterministic structural choices.
pub struct ByteCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub const fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub fn next_u8(&mut self) -> u8 {
        if let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            b
        } else {
            0
        }
    }

    pub fn next_bool(&mut self) -> bool {
        self.next_u8() & 1 != 0
    }

    pub fn next_u16(&mut self) -> u16 {
        u16::from_le_bytes([self.next_u8(), self.next_u8()])
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut buf = [0_u8; 8];
        for slot in &mut buf {
            *slot = self.next_u8();
        }
        u64::from_le_bytes(buf)
    }

    /// Returns a value in `0..upper` (or `0` when `upper == 0`).
    pub fn range_usize(&mut self, upper: usize) -> usize {
        if upper == 0 {
            return 0;
        }
        if upper <= 256 {
            usize::from(self.next_u8()) % upper
        } else {
            usize::from(self.next_u16()) % upper
        }
    }

    /// Generates a `u64` spanning 1-byte, medium, and full 64-bit magnitudes so multi-byte
    /// and >5-byte varints get regular coverage.
    pub fn next_varied_u64(&mut self) -> u64 {
        match self.next_u8() & 0x03 {
            0 => u64::from(self.next_u8() & 0x3F),
            1 => u64::from(self.next_u16()),
            2 => (u64::from(self.next_u16()) << 16) | u64::from(self.next_u16()),
            _ => self.next_u64(),
        }
    }

    /// Generates an `i64` spanning small positive/negative values and full 64-bit extremes.
    pub fn next_varied_i64(&mut self) -> i64 {
        self.next_varied_u64().cast_signed()
    }
}

/// Minimal Thrift compact-protocol encoder writing into a `Vec<u8>`.
#[derive(Default)]
pub struct ThriftWriter {
    buf: Vec<u8>,
}

impl ThriftWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn push_byte(&mut self, byte: u8) {
        self.buf.push(byte);
    }

    pub fn write_varint_u64(&mut self, mut value: u64) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            self.buf.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    pub fn write_varint_u32(&mut self, value: u32) {
        self.write_varint_u64(u64::from(value));
    }

    pub fn write_zigzag_i16(&mut self, value: i16) {
        let encoded = u16::from_ne_bytes(((value << 1) ^ (value >> 15)).to_ne_bytes());
        self.write_varint_u64(u64::from(encoded));
    }

    pub fn write_zigzag_i32(&mut self, value: i32) {
        let encoded = u32::from_ne_bytes(((value << 1) ^ (value >> 31)).to_ne_bytes());
        self.write_varint_u32(encoded);
    }

    pub fn write_zigzag_i64(&mut self, value: i64) {
        let encoded = u64::from_ne_bytes(((value << 1) ^ (value >> 63)).to_ne_bytes());
        self.write_varint_u64(encoded);
    }

    /// Writes a struct field header for `id` (with `wire_type` in `1..=13`) relative to
    /// `last_id`, optionally forcing long-delta form to exercise `skip_long_delta_field_cold`.
    pub fn write_field_header(
        &mut self,
        last_id: &mut i16,
        id: i16,
        wire_type: u8,
        force_long: bool,
    ) {
        let delta = id.checked_sub(*last_id);
        if !force_long
            && let Some(d) = delta
            && (1..=15).contains(&d)
        {
            let nibble = u8::try_from(d).expect("1..=15 fits in u8");
            self.buf.push((nibble << 4) | (wire_type & 0x0F));
        } else {
            self.buf.push(wire_type & 0x0F);
            self.write_zigzag_i16(id);
        }
        *last_id = id;
    }

    pub fn write_stop(&mut self) {
        self.buf.push(0);
    }

    pub fn write_binary(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("fuzz binary fits in u32");
        self.write_varint_u32(len);
        self.buf.extend_from_slice(bytes);
    }

    pub fn write_str(&mut self, text: &str) {
        self.write_binary(text.as_bytes());
    }

    pub fn write_list_header(&mut self, elem_wire_type: u8, size: u32) {
        if size < 15 {
            let nibble = u8::try_from(size).expect("size < 15 fits in u8");
            self.buf.push((nibble << 4) | (elem_wire_type & 0x0F));
        } else {
            self.buf.push(0xF0 | (elem_wire_type & 0x0F));
            self.write_varint_u32(size);
        }
    }

    pub fn write_map_header(&mut self, key_wire_type: u8, val_wire_type: u8, size: u32) {
        self.write_varint_u32(size);
        if size > 0 {
            self.buf
                .push(((key_wire_type & 0x0F) << 4) | (val_wire_type & 0x0F));
        }
    }

    /// Optionally mutates a generated payload (bit-flip, byte overwrite, or truncation)
    /// when `cursor` chooses to inject an error, exercising error/rollback/diagnose paths
    /// on deep valid structures.
    pub fn maybe_corrupt(&mut self, cursor: &mut ByteCursor<'_>) {
        if self.buf.is_empty() {
            return;
        }
        match cursor.next_u8() {
            // ~12% chance of injecting a targeted corruption on an otherwise valid payload.
            0..=10 => {
                let idx = cursor.range_usize(self.buf.len());
                let mask = cursor.next_u8() | 1;
                self.buf[idx] ^= mask;
            }
            11..=20 => {
                let new_len = cursor.range_usize(self.buf.len());
                self.buf.truncate(new_len);
            }
            21..=30 => {
                let idx = cursor.range_usize(self.buf.len());
                self.buf[idx] = cursor.next_u8();
            }
            _ => {}
        }
    }
}
