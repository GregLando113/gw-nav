//! Decompressor for Guild Wars fileserver / Gw.dat data.
//!
//! Ported from Gw.exe (build 38974) with Ghidra. The names in brackets are the
//! client's functions:
//!
//! - [`decompress`] / [`decompress_delta`]: `CmpDecompress` @ 0x0046b040
//! - full stream: `DecompressFileFull` @ 0x00469b80
//! - delta stream: `DecompressFileDelta` @ 0x00468870
//! - [`HuffTable::read`]: `HuffTable::ctor` @ 0x004664f0
//!
//! The format is LZ77 + canonical Huffman, close to deflate, read MSB-first
//! from a stream of little-endian u32 words. The top nibble of the first word
//! selects a full stream (0) or a delta stream (1). A delta stream rebuilds a
//! file from the previous version of that file, adding "copy from the old
//! file" commands to the usual literals and back-references.
//!
//! Where the client asserts, reads out of bounds, or carries on with an
//! empty Huffman table after bad code lengths, this returns an error.

/// End-of-list marker for the per-length symbol lists, and the fast-table
/// bit count that sends a lookup to the slow path.
const NONE: u32 = u32::MAX;

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum DecompressError {
    #[error("compressed input ({input} bytes) is larger than the output ({output} bytes)")]
    InputTooLarge { input: usize, output: usize },
    #[error("delta-compressed data needs the previous version of the file")]
    DeltaNeedsSource,
    #[error("unknown compression format {0}")]
    UnknownFormat(u32),
    #[error("corrupt compressed data: {0}")]
    Corrupt(&'static str),
}

type Result<T> = std::result::Result<T, DecompressError>;

/// Decompress `input` into exactly `out_size` bytes.
///
/// Delta-compressed input fails with [`DecompressError::DeltaNeedsSource`];
/// use [`decompress_delta`] for that.
pub fn decompress(input: &[u8], out_size: usize) -> Result<Vec<u8>> {
    cmp_decompress(input, out_size, None)
}

/// Decompress `input` into exactly `out_size` bytes, taking the previous
/// version of the file as `old_source` for delta-compressed input. Full
/// streams decode as with [`decompress`] and ignore `old_source`.
pub fn decompress_delta(input: &[u8], out_size: usize, old_source: &[u8]) -> Result<Vec<u8>> {
    cmp_decompress(input, out_size, Some(old_source))
}

fn cmp_decompress(input: &[u8], out_size: usize, old_source: Option<&[u8]>) -> Result<Vec<u8>> {
    if input.len() > out_size {
        return Err(DecompressError::InputTooLarge {
            input: input.len(),
            output: out_size,
        });
    }
    // Stored uncompressed.
    if input.len() == out_size {
        return Ok(input.to_vec());
    }

    let (mut bs, format) = BitStream::new(input);
    let mut out = vec![0u8; out_size];
    match format {
        0 => decompress_full(&mut bs, &mut out)?,
        1 => {
            let old = old_source.ok_or(DecompressError::DeltaNeedsSource)?;
            decompress_file_delta(&mut bs, &mut out, old)?
        }
        f => return Err(DecompressError::UnknownFormat(f)),
    }
    Ok(out)
}

/// `DecompressFileFull`: literals and back-references into the output.
fn decompress_full(bs: &mut BitStream, out: &mut [u8]) -> Result<()> {
    let match_bonus = bs.read(4);
    let mut pos = 0;
    while pos < out.len() {
        let literals = HuffTable::read(bs)?;
        let distances = HuffTable::read(bs)?;
        let symbol_count = (bs.read(4) + 1) << 12;
        for _ in 0..symbol_count {
            if pos == out.len() {
                break;
            }
            let sym = literals.decode(bs)?;
            if sym < 0x100 {
                out[pos] = sym as u8;
                pos += 1;
                continue;
            }
            let len = match_bonus + 1 + read_length(bs, sym - 0x100)?;
            let dist_sym = distances.decode(bs)?;
            let dist = read_distance(bs, dist_sym)?;
            copy_match(out, &mut pos, len, dist)?;
        }
    }
    Ok(())
}

/// `DecompressFileDelta`: like [`decompress_full`], plus commands that copy
/// a run of bytes out of `old`.
///
/// Old-file offsets are relative to a cursor that predicts where in `old`
/// the output currently is. Every output byte advances it by one; an
/// old-file copy close to the previous one resynchronises it to the end of
/// the copied range.
fn decompress_file_delta(bs: &mut BitStream, out: &mut [u8], old: &[u8]) -> Result<()> {
    let match_bonus = bs.read(4);
    let old_match_bonus = bs.read(4);
    let resync_window = old_match_bonus + 0x100;
    let mut old_cursor = 0u32;
    let mut prev_old_pos = 0u32;
    let mut pos = 0;
    while pos < out.len() {
        let commands = HuffTable::read(bs)?;
        let distances = HuffTable::read(bs)?;
        let old_offsets = HuffTable::read(bs)?;
        let symbol_count = (bs.read(4) + 1) << 12;
        for _ in 0..symbol_count {
            if pos == out.len() {
                break;
            }
            let sym = commands.decode(bs)?;
            if sym < 0x100 {
                out[pos] = sym as u8;
                pos += 1;
                old_cursor = old_cursor.wrapping_add(1);
            } else if sym < 0x100 + LEN_SYMBOLS {
                let len = match_bonus + 1 + read_length(bs, sym - 0x100)?;
                let dist_sym = distances.decode(bs)?;
                let dist = read_distance(bs, dist_sym)?;
                copy_match(out, &mut pos, len, dist)?;
                old_cursor = old_cursor.wrapping_add(len);
            } else {
                let len = old_match_bonus + 1 + read_length(bs, sym - 0x100 - LEN_SYMBOLS)?;
                let offset_sym = old_offsets.decode(bs)?;
                let offset = read_old_offset(bs, offset_sym)?;
                let old_pos = old_cursor.wrapping_add(offset);

                let (len_us, old_us) = (len as usize, old_pos as usize);
                if pos + len_us > out.len() || old_us + len_us > old.len() {
                    return Err(DecompressError::Corrupt("old-file copy out of range"));
                }
                if old_pos.wrapping_sub(prev_old_pos) <= resync_window
                    || prev_old_pos.wrapping_sub(old_pos) <= resync_window
                {
                    old_cursor = old_pos.wrapping_add(len);
                } else {
                    old_cursor = old_cursor.wrapping_add(len);
                }
                prev_old_pos = old_pos;

                out[pos..pos + len_us].copy_from_slice(&old[old_us..old_us + len_us]);
                pos += len_us;
            }
        }
    }
    Ok(())
}

/// Back-reference: copy `len` bytes starting `dist + 1` bytes back.
fn copy_match(out: &mut [u8], pos: &mut usize, len: u32, dist: u32) -> Result<()> {
    let (len, dist) = (len as usize, dist as usize);
    if *pos + len > out.len() || dist >= *pos {
        return Err(DecompressError::Corrupt("back-reference out of range"));
    }
    // Byte by byte: source and destination may overlap.
    for _ in 0..len {
        out[*pos] = out[*pos - dist - 1];
        *pos += 1;
    }
    Ok(())
}

/// Match length (before the stream's bonus is added) for length code `code`.
fn read_length(bs: &mut BitStream, code: u32) -> Result<u32> {
    let code = code as usize;
    if code >= LEN_BASE.len() {
        return Err(DecompressError::Corrupt("bad length symbol"));
    }
    Ok(LEN_BASE[code] as u32 | bs.read(LEN_EXTRA[code] as u32))
}

/// Back-reference distance (minus one) for distance symbol `sym`.
fn read_distance(bs: &mut BitStream, sym: u32) -> Result<u32> {
    let sym = sym as usize;
    if sym >= DIST_BASE.len() {
        return Err(DecompressError::Corrupt("bad distance symbol"));
    }
    Ok(DIST_BASE[sym] as u32 | bs.read(DIST_EXTRA[sym] as u32))
}

/// Signed old-file offset for offset symbol `sym`, as a two's complement u32.
///
/// Bit 0 of the symbol is the sign. The rest picks a magnitude bucket: the
/// distance buckets up to symbol 0x3C, doubling buckets beyond.
fn read_old_offset(bs: &mut BitStream, sym: u32) -> Result<u32> {
    if sym as usize >= OFFSET_EXTRA.len() {
        return Err(DecompressError::Corrupt("bad old-file offset symbol"));
    }
    let base = if sym < 0x3C {
        DIST_BASE[(sym >> 1) as usize] as u32
    } else {
        ((sym & 2) + 4) << ((sym >> 2) - 2)
    };
    let magnitude = base | bs.read(OFFSET_EXTRA[sym as usize] as u32);
    Ok(if sym & 1 != 0 {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

/// `GuildWars::CmpApi::BitStream`: an MSB-first reader over u32 words.
///
/// `rack0` always holds the next 32 bits of the stream and `rack1` the
/// `rack1_bits` bits after those, left-aligned. Past the end of the input,
/// zero bits are shifted in.
struct BitStream<'a> {
    /// The input, truncated to whole words.
    input: &'a [u8],
    /// Byte offset of the next word to load.
    next_word: usize,
    rack0: u32,
    rack1: u32,
    rack1_bits: u32,
}

impl<'a> BitStream<'a> {
    /// Prime the racks and consume the format nibble, which is returned.
    fn new(input: &'a [u8]) -> (Self, u32) {
        let mut bs = Self {
            input: &input[..input.len() & !3],
            next_word: 0,
            rack0: 0,
            rack1: 0,
            rack1_bits: 0,
        };
        let first = bs.load_word().unwrap_or(0);
        bs.rack0 = first << 4;
        if let Some(word) = bs.load_word() {
            bs.rack0 |= word >> 28;
            bs.rack1 = word << 4;
            bs.rack1_bits = 28;
        }
        (bs, first >> 28)
    }

    fn load_word(&mut self) -> Option<u32> {
        let bytes = self.input.get(self.next_word..self.next_word + 4)?;
        self.next_word += 4;
        Some(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    /// Read `n` (< 32) bits as an integer; 0 bits read as 0.
    fn read(&mut self, n: u32) -> u32 {
        let value = if n == 0 { 0 } else { self.rack0 >> (32 - n) };
        self.consume(n);
        value
    }

    /// Drop `n` (< 32) bits from the front of the stream.
    fn consume(&mut self, n: u32) {
        debug_assert!(n < 32, "bitCount < 8 * sizeof(m_rackData0)");
        if n != 0 {
            self.rack0 = (self.rack1 >> (32 - n)) | (self.rack0 << n);
        }
        if n <= self.rack1_bits {
            self.rack1_bits -= n;
            self.rack1 <<= n;
        } else if let Some(word) = self.load_word() {
            let bits = self.rack1_bits + 32 - n;
            self.rack0 |= word >> bits;
            self.rack1 = word << (n - self.rack1_bits);
            self.rack1_bits = bits;
        } else {
            self.rack1 = 0;
            self.rack1_bits = 0;
        }
    }
}

/// A canonical Huffman decoding table, read from the stream.
///
/// Codes are assigned from the top of the code space down: shorter codes
/// first and, within a length, lower symbols first.
struct HuffTable {
    /// (bit count, symbol) indexed by the next 8 bits of the stream. A bit
    /// count of [`NONE`] means the code is longer than 8 bits.
    fast: [(u32, u32); 256],
    /// Codes longer than 8 bits, one group per length, longest last.
    slow: Vec<SlowGroup>,
    /// Symbols of the long codes, in code order (descending).
    long_symbols: Vec<u32>,
}

struct SlowGroup {
    /// The group's lowest code, left-aligned. Lookups take the first group
    /// whose threshold is at or below the next 32 bits of the stream.
    threshold: u32,
    /// Index in `long_symbols` of the symbol with the lowest code.
    last_index: u32,
    bits: u32,
}

impl HuffTable {
    /// `HuffTable::ctor`: read a symbol count and the code lengths, and build
    /// the decoding tables.
    fn read(bs: &mut BitStream) -> Result<Self> {
        let symbol_count = bs.read(16);

        // Symbols of each code length as linked lists: heads[len] is the
        // first symbol, next[sym] the one after it.
        let mut next = vec![NONE; symbol_count as usize];
        let mut heads = [NONE; 32];
        let mut total = 0u32;

        // Code lengths are themselves prefix coded (see `read_code_length`),
        // from the last symbol down, each entry covering a run of up to 8
        // symbols. A zero length means unused, unless there's only one
        // symbol.
        let mut sym = symbol_count.wrapping_sub(1);
        while sym != NONE {
            let (run, len) = read_code_length(bs);
            if run > sym {
                return Err(DecompressError::Corrupt("code length run past symbol 0"));
            }
            if len == 0 && symbol_count > 1 {
                sym = sym.wrapping_sub(run + 1);
                continue;
            }
            total += run + 1;
            for _ in 0..=run {
                next[sym as usize] = heads[len];
                heads[len] = sym;
                sym = sym.wrapping_sub(1);
            }
        }

        // No used symbol at all: the last symbol becomes a 0-bit code.
        if symbol_count != 0 && total == 0 {
            next[symbol_count as usize - 1] = heads[0];
            heads[0] = symbol_count - 1;
            total = 1;
        }

        // Codes of up to 8 bits fill every fast-table slot they prefix.
        // Slots no code covers stay (0, 0): they decode as symbol 0 and
        // consume nothing, which is what an empty table does too.
        let mut fast = [(0u32, 0u32); 256];
        let mut code = 0u32;
        let mut assigned = 0u32;
        for bits in 0..=8 {
            let mut sym = heads[bits as usize];
            while sym != NONE {
                if code >= 1 << bits {
                    return Err(DecompressError::Corrupt("oversubscribed huffman code"));
                }
                let fill = 8 - bits;
                for low in 0..1 << fill {
                    fast[(code << fill | low) as usize] = (bits, sym);
                }
                code = code.wrapping_sub(1);
                sym = next[sym as usize];
                assigned += 1;
            }
            code = code.wrapping_mul(2).wrapping_add(1);
        }

        // Longer codes mark their 8-bit prefix for the slow path.
        let mut slow = Vec::new();
        let mut long_symbols = Vec::with_capacity((total - assigned) as usize);
        for bits in 9..32 {
            let mut sym = heads[bits as usize];
            if sym != NONE {
                let mut last_code;
                loop {
                    if code >= 1 << bits {
                        return Err(DecompressError::Corrupt("oversubscribed huffman code"));
                    }
                    fast[(code >> (bits - 8)) as usize].0 = NONE;
                    long_symbols.push(sym);
                    last_code = code;
                    code = code.wrapping_sub(1);
                    sym = next[sym as usize];
                    if sym == NONE {
                        break;
                    }
                }
                slow.push(SlowGroup {
                    threshold: last_code << (32 - bits),
                    last_index: long_symbols.len() as u32 - 1,
                    bits,
                });
            }
            code = code.wrapping_mul(2).wrapping_add(1);
        }

        Ok(Self {
            fast,
            slow,
            long_symbols,
        })
    }

    /// Decode one symbol and consume its bits.
    fn decode(&self, bs: &mut BitStream) -> Result<u32> {
        let (mut bits, mut sym) = self.fast[(bs.rack0 >> 24) as usize];
        if bits == NONE {
            // The client walks off the end of the groups here when the code
            // is incomplete.
            let group = self
                .slow
                .iter()
                .find(|g| bs.rack0 >= g.threshold)
                .ok_or(DecompressError::Corrupt("incomplete huffman code"))?;
            let index = group
                .last_index
                .wrapping_sub((bs.rack0 - group.threshold) >> (32 - group.bits));
            sym = *self
                .long_symbols
                .get(index as usize)
                .ok_or(DecompressError::Corrupt("bad long huffman code"))?;
            bits = group.bits;
        }
        bs.consume(bits);
        Ok(sym)
    }
}

/// Read one code-length entry: `(run, len)` for `run + 1` symbols of code
/// length `len`. The entries use a fixed prefix code of 3 to 16 bits.
fn read_code_length(bs: &mut BitStream) -> (u32, usize) {
    // The last threshold is 0, so this always finds one.
    let group = CL_CODE.iter().position(|&(t, _)| bs.rack0 >= t).unwrap();
    let (threshold, last_index) = CL_CODE[group];
    let bits = group as u32 + 3;
    let index = last_index as u32 - ((bs.rack0 - threshold) >> (32 - bits));
    let entry = CL_SYMBOLS[index as usize];
    bs.consume(bits);
    ((entry >> 5) as u32, (entry & 0x1F) as usize)
}

// Tables from Gw.exe build 38974.

/// The code-length prefix code: (threshold, index in `CL_SYMBOLS` of the
/// lowest code) for code lengths 3, 4, ... 16. @ 0x0093d2c8
const CL_CODE: [(u32, u8); 14] = [
    (0xA000_0000, 0x02),
    (0x6000_0000, 0x06),
    (0x4000_0000, 0x0A),
    (0x2000_0000, 0x12),
    (0x1200_0000, 0x19),
    (0x0C00_0000, 0x1F),
    (0x0700_0000, 0x29),
    (0x0300_0000, 0x39),
    (0x0160_0000, 0x46),
    (0x00F0_0000, 0x4D),
    (0x00C0_0000, 0x53),
    (0x00B0_0000, 0x57),
    (0x00A0_0000, 0x5F),
    (0x0000_0000, 0xFF),
];

/// Code-length entries, `run << 5 | len`, in code order. @ 0x0093d338
const CL_SYMBOLS: [u8; 256] = [
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

/// Number of length codes. Literal/command symbols 0x100.. are new-output
/// copies; in delta streams the next `LEN_SYMBOLS` are old-file copies.
const LEN_SYMBOLS: u32 = 29;

/// Length base per length code (deflate's, minus 3). @ 0x0093d894
const LEN_BASE: [u8; LEN_SYMBOLS as usize] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224, 255,
];

/// Extra bits per length code. @ 0x0093da74
const LEN_EXTRA: [u8; LEN_SYMBOLS as usize] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Distance base per distance symbol (deflate's, minus 1). @ 0x0093d858
const DIST_BASE: [u16; 30] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576,
];

/// Extra bits per distance symbol. @ 0x0093d9d8
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Extra bits per old-file offset symbol. @ 0x0093d9f8
const OFFSET_EXTRA: [u8; 124] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6,
    7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13,
    13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18, 19, 19, 19,
    19, 20, 20, 20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 23, 23, 23, 23, 24, 24, 24, 24, 25, 25, 25,
    25, 26, 26, 26, 26, 27, 27, 27, 27, 28, 28, 28, 28, 29, 29, 29, 29,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Reverse;
    use std::collections::{BinaryHeap, HashMap};

    fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn empty_output() {
        assert_eq!(decompress(&[], 0), Ok(Vec::new()));
        assert_eq!(
            decompress(&[1, 2, 3], 0),
            Err(DecompressError::InputTooLarge {
                input: 3,
                output: 0
            })
        );
    }

    #[test]
    fn stored_uncompressed() {
        let data = b"ffna\x03 stored as-is".to_vec();
        assert_eq!(decompress(&data, data.len()).unwrap(), data);
    }

    #[test]
    fn format_nibble() {
        let delta = words_to_bytes(&[0x1000_0000, 0, 0]);
        assert_eq!(
            decompress(&delta, 100),
            Err(DecompressError::DeltaNeedsSource)
        );
        let bad = words_to_bytes(&[0x2000_0000, 0, 0]);
        assert_eq!(
            decompress(&bad, 100),
            Err(DecompressError::UnknownFormat(2))
        );
    }

    #[test]
    fn exhausted_input_terminates() {
        // Zero-symbol tables decode every symbol as a 0-bit literal 0, as in
        // the client; the point is that this terminates.
        assert_eq!(decompress(&[0; 64], 100_000), Ok(vec![0; 100_000]));
        assert_eq!(decompress(&[], 1000), Ok(vec![0; 1000]));
    }

    #[test]
    fn garbage_never_panics() {
        // Simple xorshift so the test is deterministic without extra deps.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let old = vec![0xAB; 5000];
        for len in [8usize, 16, 64, 256, 4096] {
            for i in 0..400 {
                let mut buf = Vec::with_capacity(len);
                while buf.len() < len {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    buf.extend_from_slice(&state.to_le_bytes());
                }
                buf.truncate(len);
                buf[3] = (buf[3] & 0x0F) | (i as u8 & 1) << 4; // full or delta
                let _ = decompress_delta(&buf, 10_000, &old);
            }
        }
    }

    /// Checks every `testdata/<id>.cmp` against `testdata/<id>.mapblob`.
    /// Real game data isn't committed; populate with
    /// `cargo run -- download <ids> --raw --out-dir testdata`.
    #[test]
    fn golden_files() {
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata"));
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
            assert!(
                output == expected,
                "{} decompressed differently",
                cmp_path.display()
            );
        }
    }

    #[test]
    fn bit_stream_refills_across_words() {
        let input = words_to_bytes(&[0x0123_4567, 0x89AB_CDEF, 0xFEDC_BA98]);
        let (mut bs, format) = BitStream::new(&input);
        assert_eq!(format, 0);
        // Stream after the format nibble: 1234567 89ABCDEF FEDCBA98
        assert_eq!(bs.rack0, 0x1234_5678);
        assert_eq!(bs.read(16), 0x1234);
        assert_eq!(bs.rack0, 0x5678_9ABC);
        bs.consume(28);
        assert_eq!(bs.rack0, 0xCDEF_FEDC);
        bs.consume(24);
        assert_eq!(bs.rack0, 0xDCBA_9800);
        bs.consume(20);
        assert_eq!(bs.rack0, 0x8000_0000);
        bs.consume(4);
        assert_eq!(bs.rack0, 0);
    }

    /// The code-length prefix code is complete and covers all 256 entries.
    #[test]
    fn code_length_code_is_a_bijection() {
        let codes = code_length_codes();
        assert_eq!(codes.len(), 256);
        let kraft: f64 = codes
            .values()
            .map(|&(_, bits)| 0.5f64.powi(bits as i32))
            .sum();
        assert_eq!(kraft, 1.0);
    }

    // A minimal encoder for the same format, to exercise the decoder on
    // streams the client would also accept: long codes, back-references,
    // old-file copies and multiple blocks.

    /// MSB-first bit writer producing little-endian u32 words.
    #[derive(Default)]
    struct BitWriter {
        words: Vec<u32>,
        acc: u64,
        bits: u32,
    }

    impl BitWriter {
        fn put(&mut self, value: u32, bits: u32) {
            assert!(bits <= 32 && (bits == 32 || value >> bits == 0));
            self.acc = (self.acc << bits) | value as u64;
            self.bits += bits;
            if self.bits >= 32 {
                self.bits -= 32;
                self.words.push((self.acc >> self.bits) as u32);
                self.acc &= (1 << self.bits) - 1;
            }
        }

        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                self.words.push((self.acc << (32 - self.bits)) as u32);
            }
            words_to_bytes(&self.words)
        }
    }

    /// Code-length entry → (code, bits), by decoding every 16-bit prefix.
    fn code_length_codes() -> HashMap<u8, (u32, u32)> {
        let mut codes = HashMap::new();
        for prefix in 0..=0xFFFFu32 {
            // The prefix goes right after the format nibble.
            let input = words_to_bytes(&[prefix << 12, 0]);
            let (mut bs, _) = BitStream::new(&input);
            let (run, len) = read_code_length(&mut bs);
            let entry = (run << 5) as u8 | len as u8;
            let group = CL_CODE
                .iter()
                .position(|&(t, _)| prefix << 16 >= t)
                .unwrap();
            let bits = group as u32 + 3;
            codes.insert(entry, (prefix >> (16 - bits), bits));
        }
        codes
    }

    /// Huffman code lengths for `freqs` (0 for unused symbols).
    fn huffman_lengths(freqs: &[u32]) -> Vec<usize> {
        let mut lengths = vec![0; freqs.len()];
        let mut heap: BinaryHeap<Reverse<(u64, Vec<usize>)>> = (0..freqs.len())
            .filter(|&s| freqs[s] > 0)
            .map(|s| Reverse((freqs[s] as u64, vec![s])))
            .collect();
        while heap.len() > 1 {
            let Reverse((wa, mut a)) = heap.pop().unwrap();
            let Reverse((wb, b)) = heap.pop().unwrap();
            a.extend(b);
            for &s in &a {
                lengths[s] += 1;
            }
            heap.push(Reverse((wa + wb, a)));
        }
        lengths
    }

    /// An encoder-side Huffman table: (code, bits) per symbol.
    struct Code(Vec<(u32, u32)>);

    impl Code {
        /// Write the table for symbol frequencies `freqs` and return the codes.
        fn write(w: &mut BitWriter, cl: &HashMap<u8, (u32, u32)>, freqs: &[u32]) -> Code {
            let used: Vec<usize> = (0..freqs.len()).filter(|&s| freqs[s] > 0).collect();
            let mut lengths = huffman_lengths(freqs);
            // One used symbol: it must be the last, with every length 0.
            let count = used.last().map_or(0, |&s| s + 1);
            lengths.truncate(count);
            w.put(count as u32, 16);

            let mut sym = count;
            while sym > 0 {
                let len = lengths[sym - 1];
                let mut run = 1;
                while run < 8 && sym > run && lengths[sym - 1 - run] == len {
                    run += 1;
                }
                let (code, bits) = cl[&(((run - 1) << 5 | len) as u8)];
                w.put(code, bits);
                sym -= run;
            }

            // Mirror of the decoder's canonical assignment.
            let mut codes = vec![(0, 0); count];
            let mut code = 0u32;
            for bits in 0..32 {
                for s in 0..count {
                    if lengths[s] == bits && (bits > 0 || used.len() == 1 && s == count - 1) {
                        codes[s] = (code, bits as u32);
                        code = code.wrapping_sub(1);
                    }
                }
                code = code.wrapping_mul(2).wrapping_add(1);
            }
            Code(codes)
        }

        fn put(&self, w: &mut BitWriter, sym: u32) {
            let (code, bits) = self.0[sym as usize];
            w.put(code, bits);
        }
    }

    #[derive(Clone, Copy)]
    enum Op {
        Literal(u8),
        /// Back-reference `dist` bytes back.
        Copy {
            len: u32,
            dist: u32,
        },
        /// Copy from the old file at `pos`.
        Old {
            len: u32,
            pos: u32,
        },
    }

    /// (symbol, extra bits value, extra bit count) for `value` in a
    /// base/extra-bits table.
    fn bucket(value: u32, base: impl Fn(usize) -> u32, extra: &[u8]) -> (u32, u32, u32) {
        (0..extra.len())
            .find(|&i| value >= base(i) && value - base(i) < 1 << extra[i])
            .map(|i| (i as u32, value - base(i), extra[i] as u32))
            .unwrap()
    }

    fn old_offset_base(sym: usize) -> u32 {
        if sym < 0x3C {
            DIST_BASE[sym >> 1] as u32
        } else {
            ((sym as u32 & 2) + 4) << ((sym >> 2) - 2)
        }
    }

    const MATCH_BONUS: u32 = 2;
    const OLD_MATCH_BONUS: u32 = 3;

    /// One encoded op: (command symbol, extra bits, distance/offset symbol
    /// and its extra bits).
    struct Encoded {
        command: u32,
        len_extra: (u32, u32),
        tail: Option<(u32, u32, u32)>,
        old: bool,
    }

    /// Encode `ops`, at most 4096 per block. `delta` selects the format.
    fn encode(ops: &[Op], delta: bool) -> Vec<u8> {
        let cl = code_length_codes();
        let mut w = BitWriter::default();
        w.put(delta as u32, 4);
        w.put(MATCH_BONUS, 4);
        if delta {
            w.put(OLD_MATCH_BONUS, 4);
        }

        let (mut cursor, mut prev) = (0u32, 0u32);
        let encoded: Vec<Encoded> = ops
            .iter()
            .map(|&op| match op {
                Op::Literal(b) => {
                    cursor = cursor.wrapping_add(1);
                    Encoded {
                        command: b as u32,
                        len_extra: (0, 0),
                        tail: None,
                        old: false,
                    }
                }
                Op::Copy { len, dist } => {
                    cursor = cursor.wrapping_add(len);
                    let (ls, lv, lb) =
                        bucket(len - MATCH_BONUS - 1, |i| LEN_BASE[i] as u32, &LEN_EXTRA);
                    let (ds, dv, db) = bucket(dist - 1, |i| DIST_BASE[i] as u32, &DIST_EXTRA);
                    Encoded {
                        command: 0x100 + ls,
                        len_extra: (lv, lb),
                        tail: Some((ds, dv, db)),
                        old: false,
                    }
                }
                Op::Old { len, pos } => {
                    let (ls, lv, lb) = bucket(
                        len - OLD_MATCH_BONUS - 1,
                        |i| LEN_BASE[i] as u32,
                        &LEN_EXTRA,
                    );
                    let offset = pos.wrapping_sub(cursor) as i32;
                    let sign = (offset < 0) as u32;
                    let (os, ov, ob) =
                        bucket(offset.unsigned_abs(), old_offset_base, &OFFSET_EXTRA);
                    assert_eq!(os & 1, 0);
                    let window = OLD_MATCH_BONUS + 0x100;
                    cursor = if pos.wrapping_sub(prev) <= window || prev.wrapping_sub(pos) <= window
                    {
                        pos + len
                    } else {
                        cursor.wrapping_add(len)
                    };
                    prev = pos;
                    Encoded {
                        command: 0x100 + LEN_SYMBOLS + ls,
                        len_extra: (lv, lb),
                        tail: Some((os | sign, ov, ob)),
                        old: true,
                    }
                }
            })
            .collect();

        for block in encoded.chunks(4096) {
            let mut command_freqs = vec![0u32; 0x100 + 2 * LEN_SYMBOLS as usize];
            let mut dist_freqs = vec![0u32; DIST_BASE.len()];
            let mut offset_freqs = vec![0u32; OFFSET_EXTRA.len()];
            for e in block {
                command_freqs[e.command as usize] += 1;
                if let Some((s, _, _)) = e.tail {
                    let freqs = if e.old {
                        &mut offset_freqs
                    } else {
                        &mut dist_freqs
                    };
                    freqs[s as usize] += 1;
                }
            }
            let commands = Code::write(&mut w, &cl, &command_freqs);
            let distances = Code::write(&mut w, &cl, &dist_freqs);
            let offsets = delta.then(|| Code::write(&mut w, &cl, &offset_freqs));
            w.put(0, 4); // up to 4096 symbols in this block
            for e in block {
                commands.put(&mut w, e.command);
                w.put(e.len_extra.0, e.len_extra.1);
                if let Some((s, v, b)) = e.tail {
                    let table = if e.old {
                        offsets.as_ref().unwrap()
                    } else {
                        &distances
                    };
                    table.put(&mut w, s);
                    w.put(v, b);
                }
            }
        }
        w.finish()
    }

    /// What `ops` should decode to.
    fn apply(ops: &[Op], old: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &op in ops {
            match op {
                Op::Literal(b) => out.push(b),
                Op::Copy { len, dist } => {
                    for _ in 0..len {
                        out.push(out[out.len() - dist as usize]);
                    }
                }
                Op::Old { len, pos } => {
                    out.extend_from_slice(&old[pos as usize..(pos + len) as usize]);
                }
            }
        }
        out
    }

    fn xorshift(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    /// Skewed literals (so some codes run past 8 bits), runs and matches,
    /// over more than one block.
    fn full_ops() -> Vec<Op> {
        let mut rng = 0x1234_5678u32;
        let mut ops = Vec::new();
        let mut produced = 0u32;
        for i in 0..9000 {
            let r = xorshift(&mut rng);
            if produced > 40 && r % 5 == 0 {
                let len = MATCH_BONUS + 1 + r % 256;
                let dist = 1 + (r >> 9) % produced.min(32768);
                ops.push(Op::Copy { len, dist });
                produced += len;
            } else {
                // Symbol k with weight about 1/k: long tails, long codes.
                let b = (0xFF / (1 + (r >> 8) % 255)) as u8 ^ (i as u8 & 1);
                ops.push(Op::Literal(b));
                produced += 1;
            }
        }
        ops
    }

    #[test]
    fn round_trip_full() {
        let ops = full_ops();
        let expected = apply(&ops, &[]);
        let input = encode(&ops, false);
        assert!(input.len() < expected.len());
        assert_eq!(decompress(&input, expected.len()).unwrap(), expected);
        // Full streams ignore the old source.
        assert_eq!(
            decompress_delta(&input, expected.len(), b"unused").unwrap(),
            expected
        );
    }

    #[test]
    fn round_trip_delta() {
        let mut rng = 0x0BAD_F00Du32;
        let old: Vec<u8> = (0..200_000).map(|_| xorshift(&mut rng) as u8).collect();
        let mut ops = Vec::new();
        let mut old_pos = 0u32;
        for _ in 0..3000 {
            let r = xorshift(&mut rng);
            match r % 4 {
                // Mostly sequential copies with small edits, the case the
                // cursor is built for.
                0 | 1 => {
                    let len = OLD_MATCH_BONUS + 1 + (r >> 4) % 200;
                    let skip = (r >> 12) % 64;
                    if old_pos + skip + len <= old.len() as u32 {
                        old_pos += skip;
                        ops.push(Op::Old { len, pos: old_pos });
                        old_pos += len;
                    }
                }
                // Far jumps, forwards and backwards.
                2 => {
                    let len = OLD_MATCH_BONUS + 1 + (r >> 4) % 50;
                    let pos = (r >> 8) % (old.len() as u32 - len);
                    ops.push(Op::Old { len, pos });
                }
                _ => ops.push(Op::Literal(r as u8)),
            }
            if r % 7 == 0 && !ops.is_empty() {
                ops.push(Op::Copy {
                    len: MATCH_BONUS + 1 + r % 10,
                    dist: 1,
                });
            }
        }

        let expected = apply(&ops, &old);
        let input = encode(&ops, true);
        assert_eq!(
            decompress_delta(&input, expected.len(), &old).unwrap(),
            expected
        );
        assert_eq!(
            decompress(&input, expected.len()),
            Err(DecompressError::DeltaNeedsSource)
        );
        // A truncated old file makes the copies fail instead of panicking.
        assert!(matches!(
            decompress_delta(&input, expected.len(), &old[..1000]),
            Err(DecompressError::Corrupt(_))
        ));
    }

    /// Doubling weights give codes of 1 to 20 bits, so most go through the
    /// slow path.
    #[test]
    fn long_codes() {
        let cl = code_length_codes();
        let mut freqs = vec![0u32; 300];
        let symbols: Vec<u32> = (0..20).map(|i| i * 15).chain([299]).collect();
        for (i, &s) in symbols.iter().enumerate() {
            freqs[s as usize] = 1 << i.min(19);
        }
        freqs[299] = 1;

        let mut w = BitWriter::default();
        w.put(0, 4);
        let code = Code::write(&mut w, &cl, &freqs);
        assert_eq!(code.0.iter().map(|&(_, bits)| bits).max(), Some(20));
        for &s in symbols.iter().chain(symbols.iter().rev()) {
            code.put(&mut w, s);
        }
        let input = w.finish();

        let (mut bs, _) = BitStream::new(&input);
        let table = HuffTable::read(&mut bs).unwrap();
        assert_eq!(table.slow.len(), 12);
        for &s in symbols.iter().chain(symbols.iter().rev()) {
            assert_eq!(table.decode(&mut bs), Ok(s));
        }
    }

    /// One used symbol: written with every length 0, decoded as 0 bits.
    #[test]
    fn single_symbol_table() {
        let ops = vec![Op::Literal(b'x'); 100];
        let input = encode(&ops, false);
        assert_eq!(decompress(&input, 100).unwrap(), vec![b'x'; 100]);
    }
}
