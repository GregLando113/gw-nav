//! Decompressor for Guild Wars fileserver / Gw.dat data.
//!
//! This is a port of the reverse-engineered `xentax.c` in
//! `references/file/compression/src/`. The format is a deflate-like LZ77 +
//! Huffman scheme read as a stream of little-endian u32 words, MSB first.
//! The control flow deliberately mirrors the C code (including its unsigned
//! wrap-around arithmetic) so it can be checked against it line by line; the
//! C variable each local corresponds to is noted where it helps.
//!
//! Differences from the C code:
//! - Paths that would read out of bounds or loop forever return an error.
//! - The leading 4-bit "integrity" nibble is checked (0 = normal, 1 = delta)
//!   as in `wip/CmpDecompress.cpp`, and input with compressed size equal to
//!   the output size is treated as stored uncompressed.

/// C's `-1` for unsigned values: end-of-list marker and "long code" marker.
const SENTINEL: u32 = u32::MAX;

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum DecompressError {
    #[error("compressed input is too short ({0} bytes)")]
    TooShort(usize),
    #[error("delta-compressed data is not supported")]
    DeltaUnsupported,
    #[error("corrupt compressed data: {0}")]
    Corrupt(&'static str),
}

type Result<T> = std::result::Result<T, DecompressError>;

/// Decompress `input` into exactly `out_size` bytes.
pub fn decompress(input: &[u8], out_size: usize) -> Result<Vec<u8>> {
    if out_size == 0 {
        return Ok(Vec::new());
    }
    if input.len() == out_size {
        return Ok(input.to_vec());
    }

    // Trailing bytes that don't fill a whole word are ignored, as in C.
    let words: Vec<u32> = input
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    if words.len() < 2 {
        return Err(DecompressError::TooShort(input.len()));
    }
    match words[0] >> 28 {
        0 => {}
        1 => return Err(DecompressError::DeltaUnsupported),
        _ => return Err(DecompressError::Corrupt("bad integrity nibble")),
    }

    let mut br = BitReader::new(&words);
    let match_bonus = br.cur >> 28; // EBPminus18
    br.consume(4);

    let mut out = vec![0u8; out_size];
    let mut pos = 0usize;
    let mut lit_tree = HuffmanTree::new(); // HuffmanTree
    let mut dist_tree = HuffmanTree::new(); // HuffmanTree2

    while pos < out_size {
        let block_start = pos;
        lit_tree.setup(&mut br)?;
        dist_tree.setup(&mut br)?;

        let count = br.cur >> 28; // _counter1
        br.consume(4);

        for _ in 0..(count + 1) << 12 {
            if pos == out_size {
                break;
            }
            let sym = lit_tree.decode(&mut br)?;
            if sym < 0x100 {
                out[pos] = sym as u8;
                pos += 1;
                continue;
            }

            let extra = table4(sym)?;
            let mut len = table3(sym)?;
            if extra != 0 {
                len |= br.peek(extra)?;
                br.consume(extra);
            }
            let len = (match_bonus + len + 1) as usize; // EBPminus1c

            let dsym = dist_tree.decode(&mut br)?;
            let extra = table5(dsym)?;
            let mut dist = table6(dsym)?; // backtrack
            if extra != 0 {
                dist |= br.peek(extra)?;
                br.consume(extra);
            }
            let dist = dist as usize;

            if pos + len > out_size || dist >= pos {
                return Err(DecompressError::Corrupt("back-reference out of range"));
            }
            // Byte-by-byte because source and destination may overlap.
            for _ in 0..len {
                out[pos] = out[pos - dist - 1];
                pos += 1;
            }
        }

        if pos == block_start {
            return Err(DecompressError::Corrupt("block produced no output"));
        }
    }
    Ok(out)
}

/// MSB-first bit reader over u32 words.
///
/// `cur` always holds the next 32 bits of the stream; `next` holds the
/// `avail` bits after those, left-aligned.
struct BitReader<'a> {
    words: &'a [u32],
    pos: usize,
    cur: u32,   // ESIplusC
    next: u32,  // ESIplus10
    avail: u32, // ESIplus8
}

impl<'a> BitReader<'a> {
    /// Caller guarantees `words.len() >= 2`. Skips the 4-bit integrity nibble.
    fn new(words: &'a [u32]) -> Self {
        Self {
            words,
            pos: 2,
            cur: (words[0] << 4) | (words[1] >> 28),
            next: words[1] << 4,
            avail: 28,
        }
    }

    /// Top `n` bits of `cur` (1..=31).
    fn peek(&self, n: u32) -> Result<u32> {
        if n == 0 || n >= 32 {
            return Err(DecompressError::Corrupt("invalid bit count"));
        }
        Ok(self.cur >> (32 - n))
    }

    /// Drop `n` (< 32) bits from the front of the stream. Once the input is
    /// exhausted, zero bits are shifted in (matches the C refill logic).
    fn consume(&mut self, n: u32) {
        debug_assert!(n < 32);
        if n == 0 {
            return;
        }
        self.cur = (self.next >> (32 - n)) | (self.cur << n);
        if n > self.avail {
            if self.pos == self.words.len() {
                self.avail = 0;
                self.next = 0;
            } else {
                let word = self.words[self.pos];
                self.pos += 1;
                let avail = self.avail + 32 - n;
                self.cur |= word >> avail;
                self.next = word << (n - self.avail);
                self.avail = avail;
            }
        } else {
            self.avail -= n;
            self.next <<= n;
        }
    }
}

/// Canonical Huffman decoding table (`struct HuffmanData`).
///
/// Codes of up to 8 bits resolve directly through `table`; longer codes are
/// marked with `SENTINEL` there and resolved through `helper` + `long_syms`.
/// The struct is reused across blocks: some early exits in [`Self::setup`]
/// keep the previous block's tables, exactly like the C code.
struct HuffmanTree {
    /// Pairs of (code length, symbol) indexed by the top 8 bits.
    table: [u32; 0x200],
    /// Triples of (lower bound of left-aligned code, index into `long_syms`,
    /// code length) for codes longer than 8 bits.
    helper: [u32; 0x48],
    long_syms: Vec<u32>, // TempArray (Var2 == its length)
}

impl HuffmanTree {
    fn new() -> Self {
        Self {
            table: [0; 0x200],
            helper: [0; 0x48],
            long_syms: Vec::new(),
        }
    }

    /// Read a code-length table from the stream and build the decoding
    /// tables. Port of `SetupNodesandTree`.
    fn setup(&mut self, br: &mut BitReader) -> Result<()> {
        self.long_syms.clear();

        let n_syms = br.cur >> 16; // EBPminus14
        br.consume(16);

        // Per code length, a linked list of symbols (heads + next pointers).
        let mut next = vec![0u32; n_syms as usize]; // EBPminus20
        let mut heads = [SENTINEL; 32]; // EBPminus128 + 0x20
        let mut total = 0u32; // EBPminus1C
        let last = n_syms.wrapping_sub(1); // EBPminus10
        let mut sym = last; // EBPminus8

        // Code lengths are themselves coded with a fixed prefix code
        // (TABLE1 + TABLE2), each entry giving a run of up to 8 symbols.
        if n_syms != 0 {
            loop {
                let mut i = 0;
                while br.cur < table1(i) {
                    i += 2;
                }
                let nbits = ((i * 4 + 0x18) >> 3) as u32; // j
                let idx = table1(i + 1).wrapping_sub((br.cur - table1(i)) >> (32 - nbits));
                let entry = *TABLE2
                    .get(idx as usize)
                    .ok_or(DecompressError::Corrupt("bad code-length prefix"))?
                    as u32; // arg_0
                br.consume(nbits);

                let mut run = entry >> 5;
                let code_len = (entry & 0x1f) as usize;
                if run > sym {
                    return Ok(());
                }

                if code_len != 0 || n_syms < 2 {
                    total += run + 1;
                    loop {
                        if sym >= n_syms {
                            return Err(DecompressError::Corrupt("symbol index out of range"));
                        }
                        next[sym as usize] = heads[code_len];
                        heads[code_len] = sym;
                        sym = sym.wrapping_sub(1);
                        if run == 0 || sym == SENTINEL {
                            break;
                        }
                        run -= 1;
                    }
                } else {
                    sym = sym.wrapping_sub(run + 1);
                }

                if sym == SENTINEL {
                    break;
                }
            }
        }

        // processhuffmantree
        if n_syms != 0 && total == 0 {
            next[last as usize] = heads[0];
            heads[0] = last;
            total = 1;
        }

        self.table = [0; 0x200];

        // Assign codes of 0..=8 bits, from the top of the code space down.
        let mut bits = 0u32; // arg_0
        let mut assigned = 0u32; // EBPminus24
        let mut code = 0u32; // EBPminus8
        while bits <= 8 {
            let mut v = heads[bits as usize];
            if v != SENTINEL {
                let limit = 1u32 << bits; // EBPminus10
                loop {
                    if code >= limit || v >= n_syms {
                        return Ok(());
                    }
                    let base = code << (8 - bits); // EBPminus28
                    for i in 0..(1u32 << (8 - bits)) {
                        let idx = (base | i) as usize;
                        if idx >= 0x100 {
                            return Err(DecompressError::Corrupt("huffman table overflow"));
                        }
                        self.table[idx * 2] = bits;
                        self.table[idx * 2 + 1] = v;
                    }
                    v = next[v as usize];
                    assigned += 1;
                    code = code.wrapping_sub(1);
                    if v == SENTINEL {
                        break;
                    }
                }
            }
            code = code.wrapping_mul(2).wrapping_add(1);
            bits += 1;
        }

        if assigned > total {
            return Err(DecompressError::Corrupt("too many huffman codes"));
        }
        if assigned == total {
            return Ok(());
        }

        // Codes longer than 8 bits: mark their 8-bit prefixes and record
        // ranges in `helper`.
        let n_long = (total - assigned) as usize;
        self.long_syms = vec![0; n_long];
        let mut v = 0usize;
        let mut h = 0usize;
        while bits <= 31 {
            let mut e = heads[bits as usize];
            if e != SENTINEL {
                let limit = 1u32 << bits;
                loop {
                    if code > limit || e > n_syms {
                        return Ok(());
                    }
                    let idx = (code >> (bits - 8)) as usize * 2;
                    *self
                        .table
                        .get_mut(idx)
                        .ok_or(DecompressError::Corrupt("huffman table overflow"))? = SENTINEL;
                    if v >= n_long {
                        return Err(DecompressError::Corrupt("too many long codes"));
                    }
                    self.long_syms[v] = e;
                    e = *next
                        .get(e as usize)
                        .ok_or(DecompressError::Corrupt("symbol index out of range"))?;
                    v += 1;
                    code = code.wrapping_sub(1);
                    if e == SENTINEL {
                        break;
                    }
                }
                self.helper[h] = code.wrapping_add(1) << (32 - bits);
                self.helper[h + 1] = v as u32 - 1;
                self.helper[h + 2] = bits;
                h += 3;
            }
            bits += 1;
            code = code.wrapping_mul(2).wrapping_add(1);
        }
        Ok(())
    }

    /// Decode one symbol and consume its bits.
    fn decode(&self, br: &mut BitReader) -> Result<u32> {
        let i = (br.cur >> 24) as usize * 2;
        let mut len = self.table[i];
        let mut sym = self.table[i + 1];

        if len == SENTINEL {
            let mut k = 0;
            while br.cur < *self.helper.get(k).ok_or(DecompressError::Corrupt("bad long code"))? {
                k += 3;
            }
            len = self.helper[k + 2];
            if len == 0 || len >= 32 {
                return Err(DecompressError::Corrupt("bad long code length"));
            }
            let idx = self.helper[k + 1].wrapping_sub((br.cur - self.helper[k]) >> (32 - len));
            sym = *self
                .long_syms
                .get(idx as usize)
                .ok_or(DecompressError::Corrupt("bad long code index"))?;
        }

        if len >= 32 {
            return Err(DecompressError::Corrupt("bad code length"));
        }
        br.consume(len);
        Ok(sym)
    }
}

// Tables copied verbatim from xentax.c.

/// Pairs of (lower bound, index base) for the fixed code-length prefix code,
/// as little-endian u32s.
const TABLE1_DATA: [u8; 112] = [
    0x00, 0x00, 0x00, 0xA0, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x60, 0x06, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x12, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x12, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x1F, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x07, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x39, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x60, 0x01, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x00, 0x4D, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xC0, 0x00, 0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0xB0, 0x00, 0x57, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xA0, 0x00, 0x5F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00,
];

/// Code-length entries: `(run << 5) | code_len`.
const TABLE2: [u8; 256] = [
    0x08, 0x09, 0x0A, 0x00, 0x07, 0x0B, 0x0C, 0x06, 0x29, 0x2A, 0xE0, 0x04, 0x05, 0x20, 0x28, 0x2B,
    0x2C, 0x40, 0x4A, 0x03, 0x0D, 0x25, 0x26, 0x27, 0x48, 0x49, 0x24, 0x47, 0x4B, 0x4C, 0x69, 0x6A,
    0x23, 0x46, 0x60, 0x63, 0x67, 0x68, 0x88, 0x89, 0xA0, 0xE8, 0x01, 0x02, 0x2D, 0x43, 0x44, 0x45,
    0x65, 0x66, 0x80, 0x87, 0x8A, 0xA8, 0xA9, 0xC0, 0xC9, 0xE9, 0x0E, 0x4D, 0x64, 0x6B, 0x6C, 0x84,
    0x85, 0x8B, 0xA4, 0xA5, 0xAA, 0xC8, 0xE5, 0x83, 0x86, 0xA6, 0xA7, 0xC7, 0xCA, 0xE7, 0x22, 0x2E,
    0x8C, 0xC4, 0xE4, 0xE6, 0x4E, 0x6D, 0xC6, 0xEC, 0x0F, 0x10, 0x11, 0x8D, 0xAB, 0xAC, 0xCC, 0xEA,
    0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x21, 0x2F,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F,
    0x41, 0x42, 0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C,
    0x5D, 0x5E, 0x5F, 0x61, 0x62, 0x6E, 0x6F, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78,
    0x79, 0x7A, 0x7B, 0x7C, 0x7D, 0x7E, 0x7F, 0x81, 0x82, 0x8E, 0x8F, 0x90, 0x91, 0x92, 0x93, 0x94,
    0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA1, 0xA2, 0xA3, 0xAD, 0xAE,
    0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE,
    0xBF, 0xC1, 0xC2, 0xC3, 0xC5, 0xCB, 0xCD, 0xCE, 0xCF, 0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
    0xD7, 0xD8, 0xD9, 0xDA, 0xDB, 0xDC, 0xDD, 0xDE, 0xDF, 0xE1, 0xE2, 0xE3, 0xEB, 0xED, 0xEE, 0xEF,
    0xF0, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF,
];

/// Match-length base values (`Table3`, indexed by symbol >= 0x100) and, at
/// offset 0x1DC, their extra-bit counts (`Table4`).
const TABLE3_DATA: [u8; 768] = [
    0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x99, 0x89, 0x80, 0x80, 0x40, 0x80, 0x40, 0x55, 0x80,
    0x42, 0x80, 0x80, 0x55, 0x80, 0x98, 0x80, 0x42, 0x80, 0x52, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
    0x55, 0x88, 0x42, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x5A, 0x80, 0x80, 0x52,
    0x80, 0x80, 0x80, 0x80, 0x92, 0x80, 0x80, 0x40, 0x80, 0x42, 0x80, 0x55, 0x80, 0x40, 0x5A, 0x80,
    0x80, 0x80, 0x80, 0x80, 0x40, 0x80, 0x80, 0x80, 0x80, 0x80, 0x99, 0x99, 0x80, 0x80, 0x89, 0x88,
    0x40, 0x40, 0x99, 0x80, 0x8A, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x90, 0x90, 0x90, 0x90, 0x99,
    0x9A, 0x99, 0x84, 0x98, 0x80, 0x80, 0x9A, 0x99, 0x80, 0x80, 0x99, 0x80, 0x99, 0x80, 0x80, 0x80,
    0x80, 0x80, 0x9A, 0x80, 0x80, 0x80, 0x8A, 0x99, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x40,
    0x40, 0x80, 0x80, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x95, 0x80, 0x80,
    0x91, 0x90, 0x80, 0x80, 0x92, 0x99, 0x80, 0x80, 0x80, 0x40, 0x40, 0x40, 0x99, 0x8A, 0x80, 0x40,
    0x4A, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
    0x07, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
    0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x06, 0x00,
    0x08, 0x00, 0x0C, 0x00, 0x10, 0x00, 0x18, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x60, 0x00,
    0x80, 0x00, 0xC0, 0x00, 0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x06,
    0x00, 0x08, 0x00, 0x0C, 0x00, 0x10, 0x00, 0x18, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x60,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0A, 0x0C, 0x0E, 0x10, 0x14, 0x18, 0x1C,
    0x20, 0x28, 0x30, 0x38, 0x40, 0x50, 0x60, 0x70, 0x80, 0xA0, 0xC0, 0xE0, 0xFF, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x04, 0x05, 0x05, 0x06, 0x06, 0x06, 0x06, 0x07, 0x07, 0x07, 0x07,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x08, 0x09, 0x09, 0x0A, 0x0A, 0x0B, 0x0B,
    0x0C, 0x0C, 0x0C, 0x0C, 0x0D, 0x0D, 0x0D, 0x0D, 0x0E, 0x0E, 0x0E, 0x0E, 0x0F, 0x0F, 0x0F, 0x0F,
    0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x12, 0x13, 0x13, 0x13, 0x13, 0x13, 0x13, 0x13, 0x13,
    0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14,
    0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15, 0x15,
    0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16, 0x16,
    0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17,
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
    0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19,
    0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19, 0x19,
    0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A,
    0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A, 0x1A,
    0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B,
    0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1B, 0x1C,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, 0x03, 0x03, 0x04, 0x04, 0x05, 0x05, 0x06, 0x06,
    0x07, 0x07, 0x08, 0x08, 0x09, 0x09, 0x0A, 0x0A, 0x0B, 0x0B, 0x0C, 0x0C, 0x0D, 0x0D, 0x0E, 0x0E,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x02, 0x02, 0x02, 0x02,
    0x03, 0x03, 0x03, 0x03, 0x04, 0x04, 0x04, 0x04, 0x05, 0x05, 0x05, 0x05, 0x06, 0x06, 0x06, 0x06,
    0x07, 0x07, 0x07, 0x07, 0x08, 0x08, 0x08, 0x08, 0x09, 0x09, 0x09, 0x09, 0x0A, 0x0A, 0x0A, 0x0A,
    0x0B, 0x0B, 0x0B, 0x0B, 0x0C, 0x0C, 0x0C, 0x0C, 0x0D, 0x0D, 0x0D, 0x0D, 0x0E, 0x0E, 0x0E, 0x0E,
    0x0F, 0x0F, 0x0F, 0x0F, 0x10, 0x10, 0x10, 0x10, 0x11, 0x11, 0x11, 0x11, 0x12, 0x12, 0x12, 0x12,
    0x13, 0x13, 0x13, 0x13, 0x14, 0x14, 0x14, 0x14, 0x15, 0x15, 0x15, 0x15, 0x16, 0x16, 0x16, 0x16,
    0x17, 0x17, 0x17, 0x17, 0x18, 0x18, 0x18, 0x18, 0x19, 0x19, 0x19, 0x19, 0x1A, 0x1A, 0x1A, 0x1A,
    0x1B, 0x1B, 0x1B, 0x1B, 0x1C, 0x1C, 0x1C, 0x1C, 0x1D, 0x1D, 0x1D, 0x1D, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x02, 0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03,
    0x04, 0x04, 0x04, 0x04, 0x05, 0x05, 0x05, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const TABLE4_OFFSET: usize = 0x1DC;

/// Extra-bit counts for distance symbols.
const TABLE5: [u8; 32] = [
    0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, 0x03, 0x03, 0x04, 0x04, 0x05, 0x05, 0x06, 0x06,
    0x07, 0x07, 0x08, 0x08, 0x09, 0x09, 0x0A, 0x0A, 0x0B, 0x0B, 0x0C, 0x0C, 0x0D, 0x0D, 0x0E, 0x0E,
];

/// Distance base values, as little-endian u16s.
const TABLE6_DATA: [u8; 92] = [
    0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x06, 0x00, 0x08, 0x00, 0x0C, 0x00,
    0x10, 0x00, 0x18, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x60, 0x00, 0x80, 0x00, 0xC0, 0x00,
    0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x06, 0x00, 0x08, 0x00, 0x0C,
    0x00, 0x10, 0x00, 0x18, 0x00, 0x20, 0x00, 0x30, 0x00, 0x40, 0x00, 0x60, 0x00, 0x01, 0x02, 0x03,
    0x04, 0x05, 0x06, 0x07, 0x08, 0x0A, 0x0C, 0x0E, 0x10, 0x14, 0x18, 0x1C, 0x20, 0x28, 0x30, 0x38,
    0x40, 0x50, 0x60, 0x70, 0x80, 0xA0, 0xC0, 0xE0, 0xFF, 0x00, 0x00, 0x00,
];

/// `i` is always <= 27: the search loop stops at the 0 bound at index 26.
fn table1(i: usize) -> u32 {
    let b = &TABLE1_DATA[i * 4..i * 4 + 4];
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn table3(sym: u32) -> Result<u32> {
    TABLE3_DATA
        .get(sym as usize)
        .map(|&v| v as u32)
        .ok_or(DecompressError::Corrupt("bad length symbol"))
}

fn table4(sym: u32) -> Result<u32> {
    TABLE3_DATA
        .get(TABLE4_OFFSET + sym as usize)
        .map(|&v| v as u32)
        .ok_or(DecompressError::Corrupt("bad length symbol"))
}

fn table5(sym: u32) -> Result<u32> {
    TABLE5
        .get(sym as usize)
        .map(|&v| v as u32)
        .ok_or(DecompressError::Corrupt("bad distance symbol"))
}

fn table6(sym: u32) -> Result<u32> {
    let i = sym as usize * 2;
    TABLE6_DATA
        .get(i..i + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
        .ok_or(DecompressError::Corrupt("bad distance symbol"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn empty_output() {
        assert_eq!(decompress(&[1, 2, 3], 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn stored_uncompressed() {
        let data = b"ffna\x03 stored as-is".to_vec();
        assert_eq!(decompress(&data, data.len()).unwrap(), data);
    }

    #[test]
    fn too_short() {
        assert_eq!(decompress(&[0; 7], 100), Err(DecompressError::TooShort(7)));
    }

    #[test]
    fn integrity_nibble() {
        let delta = words_to_bytes(&[0x1000_0000, 0, 0]);
        assert_eq!(decompress(&delta, 100), Err(DecompressError::DeltaUnsupported));
        let bad = words_to_bytes(&[0x2000_0000, 0, 0]);
        assert!(matches!(decompress(&bad, 100), Err(DecompressError::Corrupt(_))));
    }

    #[test]
    fn exhausted_input_terminates() {
        // Zero-symbol trees decode every symbol as a 0-bit literal 0 (same as
        // the C code); the point is that this terminates.
        let zeros = vec![0u8; 64];
        assert_eq!(decompress(&zeros, 100_000), Ok(vec![0; 100_000]));
    }

    #[test]
    fn garbage_never_panics() {
        // Simple xorshift so the test is deterministic without extra deps.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for len in [8usize, 16, 64, 256, 4096] {
            for _ in 0..200 {
                let mut buf = Vec::with_capacity(len);
                while buf.len() < len {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    buf.extend_from_slice(&state.to_le_bytes());
                }
                buf.truncate(len);
                buf[3] &= 0x0F; // valid integrity nibble so we get past the header
                let _ = decompress(&buf, 10_000);
            }
        }
    }

    /// Checks every `testdata/<id>.cmp` against `testdata/<id>.mapblob`.
    /// Real game data isn't committed; populate with
    /// `cargo run -- download <ids> --raw --out-dir testdata`.
    #[test]
    fn golden_files() {
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata"));
        let Ok(entries) = std::fs::read_dir(dir) else {
            eprintln!("skipping: {} not found", dir.display());
            return;
        };
        for entry in entries {
            let cmp_path = entry.unwrap().path();
            if cmp_path.extension().is_none_or(|e| e != "cmp") {
                continue;
            }
            let expected = std::fs::read(cmp_path.with_extension("mapblob")).unwrap();
            let input = std::fs::read(&cmp_path).unwrap();
            let output = decompress(&input, expected.len()).unwrap();
            assert!(output == expected, "{} decompressed differently", cmp_path.display());
        }
    }

    #[test]
    fn bit_reader_refills_across_words() {
        let words = [0x0123_4567, 0x89AB_CDEF, 0xFEDC_BA98];
        let mut br = BitReader::new(&words);
        // Stream after the skipped nibble: 1234567 89ABCDEF FEDCBA98
        assert_eq!(br.cur, 0x1234_5678);
        br.consume(16);
        assert_eq!(br.cur, 0x5678_9ABC);
        br.consume(28);
        assert_eq!(br.cur, 0xCDEF_FEDC);
        br.consume(24);
        assert_eq!(br.cur, 0xDCBA_9800);
        br.consume(20);
        assert_eq!(br.cur, 0x8000_0000);
        br.consume(4);
        assert_eq!(br.cur, 0);
    }
}
