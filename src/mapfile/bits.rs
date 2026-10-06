//! MSB-first bit reader over bytes (the client's `TrnBitStore`).
//!
//! Bytes are consumed as big-endian words. Reading past the end yields zero
//! bits, as in the client; callers that must not run dry check
//! [`BitReader::remaining`] first, as the client does.

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Bits consumed from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bits left before the end of the data.
    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// The next `n` (0..=32) bits without consuming them, zero-padded past
    /// the end.
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos / 8;
        let mut window = 0u64;
        for i in 0..5 {
            window = window << 8 | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        // `window` holds 40 bits starting at bit `byte * 8`.
        let shift = 40 - (self.pos % 8) as u32 - n;
        (window >> shift) as u32 & (u32::MAX >> (32 - n))
    }

    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// Read `n` (0..=32) bits as an unsigned value.
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }

    /// Skip to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// Bytes touched so far, counting a partly read byte.
    pub fn bytes_consumed(&self) -> usize {
        self.pos.div_ceil(8).min(self.data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let mut r = BitReader::new(&[0b1010_0000, 0xFF, 0x12, 0x34, 0x56, 0x78]);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(3), 0b010);
        assert_eq!(r.peek(8), 0b0000_1111);
        r.align();
        assert_eq!(r.read(8), 0xFF);
        assert_eq!(r.read(32), 0x12345678);
        assert_eq!(r.remaining(), 0);
        assert_eq!(r.read(5), 0); // past the end
        assert_eq!(r.bytes_consumed(), 6);
    }

    #[test]
    fn align_is_noop_on_boundary() {
        let mut r = BitReader::new(&[1, 2]);
        r.read(8);
        r.align();
        assert_eq!(r.read(8), 2);
    }
}
