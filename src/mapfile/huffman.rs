//! Canonical Huffman codes used by the terrain codecs (the client's
//! `Engine\Map\Terrain\TrnHuffman.cpp`).
//!
//! Stored in a bit stream as:
//! - `count_bits = read(8) + 1`
//! - `counts[len] = read(count_bits)` for `len` in `1..=max_len`
//! - `symbols[i] = read(symbol_bits)` for every code, in code order
//!
//! Codes are assigned canonically: in list order, counting up within a
//! length and shifting left by one between lengths. Decoding mirrors the
//! client: an 8-bit lookup table for short codes, then one bit at a time.

use super::bits::BitReader;

const FAST_BITS: u32 = 8;

#[derive(Debug, Clone)]
pub struct Huffman {
    symbols: Vec<u32>,
    /// Per 8-bit prefix: `(code length, symbol)`; length 0 = longer code.
    fast: [(u8, u32); 1 << FAST_BITS],
    /// Per length: index of its first symbol, its first code, and its last
    /// code (`None` if no codes have that length).
    first_index: Vec<u32>,
    first_code: Vec<u32>,
    last_code: Vec<Option<u32>>,
}

impl Huffman {
    /// Read a table for an alphabet of `alphabet_size` symbols with codes up
    /// to `max_len` bits. Returns `None` if the stream runs out, as the
    /// client does.
    pub fn read(br: &mut BitReader, alphabet_size: u32, max_len: usize) -> Option<Self> {
        let count_bits = br.read(8) + 1;
        let mut counts = vec![0u32; max_len + 1];
        for count in &mut counts[1..] {
            *count = br.read(count_bits);
        }
        if br.remaining() == 0 {
            return None;
        }
        let symbol_bits = (alphabet_size - 1).ilog2() + 1;
        let total: u32 = counts.iter().sum();
        let symbols: Vec<u32> = (0..total).map(|_| br.read(symbol_bits)).collect();
        Some(Self::build(&counts, symbols))
    }

    fn build(counts: &[u32], symbols: Vec<u32>) -> Self {
        let max_len = counts.len() - 1;
        let mut fast = [(0u8, 0u32); 1 << FAST_BITS];
        let mut first_index = vec![0; max_len + 1];
        let mut first_code = vec![0; max_len + 1];
        let mut last_code = vec![None; max_len + 1];

        let mut code = 0u32;
        let mut index = 0u32;
        for len in 1..=max_len {
            let count = counts[len];
            if count != 0 {
                first_index[len] = index;
                first_code[len] = code;
                last_code[len] = Some(code + count - 1);
            }
            for _ in 0..count {
                if len as u32 <= FAST_BITS {
                    let spread = FAST_BITS - len as u32;
                    let start = (code << spread) as usize;
                    for entry in &mut fast[start..start + (1 << spread)] {
                        *entry = (len as u8, symbols[index as usize]);
                    }
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        Self { symbols, fast, first_index, first_code, last_code }
    }

    /// Decode one symbol. Returns 0 if no code matches, as the client does.
    pub fn decode(&self, br: &mut BitReader) -> u32 {
        let (len, symbol) = self.fast[br.peek(FAST_BITS) as usize];
        if len != 0 {
            br.skip(len as u32);
            return symbol;
        }
        let mut code = br.read(FAST_BITS);
        for len in FAST_BITS as usize + 1..self.last_code.len() {
            code = code << 1 | br.read(1);
            if let Some(last) = self.last_code[len]
                && code <= last
            {
                let i = self.first_index[len] + code - self.first_code[len];
                return self.symbols[i as usize];
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit writer for building test streams.
    struct Bits(Vec<bool>);

    impl Bits {
        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.0.push(value >> i & 1 != 0);
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.0
                .chunks(8)
                .map(|c| c.iter().enumerate().fold(0u8, |b, (i, &bit)| b | (bit as u8) << (7 - i)))
                .collect()
        }
    }

    #[test]
    fn decodes_canonical_codes() {
        // Lengths: 'A'=1, 'B'=2, 'C'=3 and a 10-bit code for 'D', max_len 12.
        // Codes: A=0, B=10, C=110, D=1110000000.
        let mut s = Bits(Vec::new());
        s.put(3, 8); // count_bits = 4
        for len in 1..=12 {
            s.put(matches!(len, 1 | 2 | 3 | 10) as u32, 4);
        }
        for sym in [0x41, 0x42, 0x43, 0x3FF] {
            s.put(sym, 10);
        }
        for (code, len) in [(0b10, 2), (0, 1), (0b1110000000, 10), (0b110, 3)] {
            s.put(code, len);
        }
        let data = s.bytes();
        let mut br = BitReader::new(&data);
        let table = Huffman::read(&mut br, 0x400, 12).unwrap();
        let decoded: Vec<u32> = (0..4).map(|_| table.decode(&mut br)).collect();
        assert_eq!(decoded, vec![0x42, 0x41, 0x3FF, 0x43]);
    }
}
