//! Guild Wars textures (`ATEX`/`ATTX` files): the client's run-length
//! pre-pass over DXT blocks, then plain DXT1/3/5 decoding to RGBA.
//!
//! Ported from GuildWarsMapBrowser (`AtexReader.cpp`, `AtexDecompress.cpp`,
//! `AtexAsm.cpp`), which ported it from the client's assembly. Only the top
//! mip level is decoded.

use super::{RenderError, Result};

/// A decoded texture, RGBA8 row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Texture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<[u8; 4]>,
}

impl Texture {
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        self.pixels[y * self.width + x]
    }

    /// Bilinear sample with wrapping, `u`/`v` in texels.
    pub fn sample_wrapped(&self, u: f32, v: f32) -> [f32; 4] {
        let (w, h) = (self.width as i64, self.height as i64);
        let (u, v) = (u - 0.5, v - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let (fx, fy) = (u - x0, v - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let at = |x: i64, y: i64| self.pixels[(y.rem_euclid(h) * w + x.rem_euclid(w)) as usize];
        let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        std::array::from_fn(|i| {
            let top = a[i] as f32 * (1.0 - fx) + b[i] as f32 * fx;
            let bottom = c[i] as f32 * (1.0 - fx) + d[i] as f32 * fx;
            top * (1.0 - fy) + bottom * fy
        })
    }

    /// The average colour, alpha-weighted.
    pub fn average(&self) -> [f32; 4] {
        let mut sum = [0f64; 4];
        for p in &self.pixels {
            let a = p[3] as f64;
            for i in 0..3 {
                sum[i] += p[i] as f64 * a;
            }
            sum[3] += a;
        }
        let n = self.pixels.len().max(1) as f64;
        let a = sum[3].max(1.0);
        [(sum[0] / a) as f32, (sum[1] / a) as f32, (sum[2] / a) as f32, (sum[3] / n) as f32]
    }
}

/// The DXT variant of a texture file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Dxt1,
    /// DXT2, DXT3 and DXTN (normal maps).
    Dxt3,
    /// DXT4 and DXT5.
    Dxt5,
    /// DXT5 with the colour premultiplied by alpha afterwards.
    Dxtl,
}

/// Decode an `ATEX`/`ATTX` file.
pub fn decode(file: &[u8]) -> Result<Texture> {
    if file.len() < 20 {
        return Err(RenderError::Texture("short file"));
    }
    let word = |i: usize| u32::from_le_bytes(file[i..i + 4].try_into().unwrap());
    let (magic, kind) = (word(0), word(4));
    if &magic.to_le_bytes() != b"ATEX" && &magic.to_le_bytes() != b"ATTX" {
        return Err(RenderError::Texture("not an ATEX file"));
    }
    if kind & 0xFF_FFFF != u32::from_le_bytes(*b"DXT\0") {
        return Err(RenderError::Texture("not a DXT texture"));
    }
    let (format, image_format) = match (kind >> 24) as u8 {
        b'1' => (Format::Dxt1, 0x0F),
        b'2' | b'3' | b'N' => (Format::Dxt3, 0x11),
        b'4' | b'5' => (Format::Dxt5, 0x13),
        b'L' => (Format::Dxtl, 0x12),
        _ => return Err(RenderError::Texture("unsupported DXT variant")),
    };
    let (width, height) = (u16::from_le_bytes([file[8], file[9]]) as usize, u16::from_le_bytes([file[10], file[11]]) as usize);
    if width == 0 || height == 0 || width % 4 != 0 || height % 4 != 0 {
        return Err(RenderError::Texture("bad texture size"));
    }
    let words: Vec<u32> = file.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
    let blocks = decompress(&words, image_format, width, height)?;
    let mut pixels = match format {
        Format::Dxt1 => decode_dxt1(&blocks, width, height),
        Format::Dxt3 => decode_dxt3(&blocks, width, height),
        Format::Dxt5 | Format::Dxtl => decode_dxt5(&blocks, width, height),
    };
    if format == Format::Dxtl {
        for p in &mut pixels {
            for i in 0..3 {
                p[i] = (p[i] as u32 * p[3] as u32 / 255) as u8;
            }
        }
    }
    Ok(Texture { width, height, pixels })
}

/// `(shift, run - 1)` per 6-bit prefix (`byte_79053C`): the run-length code
/// of the pre-pass.
const RUN_CODES: [(u8, u8); 64] = {
    let mut t = [(1u8, 0u8); 64];
    let mut i = 0;
    while i < 16 {
        t[i] = (6, 16 - i as u8);
        i += 1;
    }
    while i < 32 {
        t[i] = (2, 0x11);
        i += 1;
    }
    t
};

/// The pre-pass's bit reader: an MSB-first 64-bit window (`current`, then
/// `next` with `remaining` valid bits) over little-endian words.
struct Bits<'a> {
    words: &'a [u32],
    pos: usize,
    current: u32,
    next: u32,
    remaining: u32,
}

impl Bits<'_> {
    /// Drop `n` (0..32) bits from the front of the window.
    fn consume(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        self.current = self.current << n | self.next >> (32 - n);
        if n > self.remaining {
            if let Some(&w) = self.words.get(self.pos) {
                self.pos += 1;
                self.current |= w >> (self.remaining + 32 - n);
                self.next = w << (n - self.remaining);
                self.remaining = self.remaining + 32 - n;
            } else {
                self.next = 0;
                self.remaining = 0;
            }
        } else {
            self.next <<= n;
            self.remaining -= n;
        }
    }

    fn take(&mut self, n: u32) -> u32 {
        let v = self.current >> (32 - n);
        self.consume(n);
        v
    }

    /// A run length and its one-bit flag.
    fn run(&mut self) -> (u32, bool) {
        let (shift, run) = RUN_CODES[(self.current >> 26) as usize];
        self.consume(shift as u32);
        (run as u32 + 1, self.take(1) != 0)
    }
}

/// A bit per block.
struct BlockSet(Vec<u32>);

impl BlockSet {
    fn new(blocks: usize) -> Self {
        Self(vec![0; blocks.div_ceil(32)])
    }
    fn get(&self, i: usize) -> bool {
        self.0[i >> 5] & 1 << (i & 31) != 0
    }
    fn set(&mut self, i: usize) {
        self.0[i >> 5] |= 1 << (i & 31);
    }
}

/// The run-length pre-pass (`AtexDecompress`), giving the DXT blocks as
/// words: 2 per block for DXT1, 4 for the others (alpha first).
fn decompress(words: &[u32], image_format: u32, width: usize, height: usize) -> Result<Vec<u32>> {
    let has_alpha = image_format != 0x0F;
    let block_size = if has_alpha { 4 } else { 2 };
    let color_offset = if has_alpha { 2 } else { 0 };
    let blocks = width * height / 16;
    let mut out = vec![0u32; blocks * block_size];
    // Blocks done by the pre-pass: alpha half (`DcmpBuffer1`) and colour
    // half (`DcmpBuffer2`).
    let mut alpha_done = BlockSet::new(blocks);
    let mut color_done = BlockSet::new(blocks);

    let data_size = *words.get(3).ok_or(RenderError::Texture("short file"))? as usize;
    let code = *words.get(4).ok_or(RenderError::Texture("short file"))?;
    if data_size <= 8 || data_size + 12 > words.len() * 4 + 3 {
        return Err(RenderError::Texture("bad data size"));
    }
    let end = (5 + (data_size - 8) / 4).min(words.len());
    let mut raw = 5;
    let mirrored = code & 0x10 != 0 && width == 256 && height == 256 && image_format == 0x11;
    if code != 0 {
        let mut bits = Bits { words: &words[..end], pos: 5, current: 0, next: 0, remaining: 0 };
        if bits.pos < end {
            bits.current = words[bits.pos];
            bits.pos += 1;
        }
        if mirrored {
            // `AtexSubCode1`: the blocks along the edges are mirrored copies.
            for i in 0..blocks {
                if is_edge(i & 31) || is_edge((i >> 6) & 31) {
                    alpha_done.set(i);
                    color_done.set(i);
                }
            }
        }
        if code & 1 != 0 && !has_alpha {
            // `AtexSubCode2`: fully transparent DXT1 blocks.
            sub_runs(&mut bits, blocks, &mut color_done, |i, done, flag| {
                if flag {
                    out[i * block_size] = 0xFFFF_FFFE;
                    out[i * block_size + 1] = 0xFFFF_FFFF;
                    done.set(i);
                    alpha_done.set(i);
                }
            });
        }
        if code & 2 != 0 && (0x10..=0x11).contains(&image_format) {
            // `AtexSubCode3`: DXT3 blocks of constant alpha.
            let a = bits.take(4);
            let pattern = (a << 4 | a) * 0x0101_0101;
            sub_alpha_runs(&mut bits, blocks, &color_done, &mut alpha_done, |i, level| {
                let v = if level == 2 { pattern } else { 0 };
                out[i * block_size] = v;
                out[i * block_size + 1] = v;
            });
        }
        if code & 4 != 0 && (0x12..=0x15).contains(&image_format) {
            // `AtexSubCode4`: DXT5 blocks of constant alpha.
            let a = bits.take(8);
            let pattern = a << 8 | a;
            sub_alpha_runs(&mut bits, blocks, &color_done, &mut alpha_done, |i, level| {
                out[i * block_size] = if level == 2 { pattern } else { 0 };
                out[i * block_size + 1] = 0;
            });
        }
        if code & 8 != 0 {
            // `AtexSubCode5`: blocks of one solid colour.
            let rgb = bits.current >> 8;
            bits.consume(24);
            let solid = solid_color_block(rgb | 0xFF00_0000, image_format == 0x0F);
            sub_runs(&mut bits, blocks, &mut color_done, |i, done, flag| {
                if flag {
                    out[i * block_size + color_offset] = solid[0];
                    out[i * block_size + color_offset + 1] = solid[1];
                    done.set(i);
                }
            });
        }
        raw = bits.pos - 1;
    }

    // The rest is stored as is: alpha halves, then colour endpoints, then
    // colour indices.
    let mut next = || {
        let w = words.get(raw).copied().unwrap_or(0);
        raw += 1;
        w
    };
    if has_alpha {
        for i in (0..blocks).filter(|&i| !alpha_done.get(i)) {
            out[i * block_size] = next();
            out[i * block_size + 1] = next();
        }
    }
    for half in 0..2 {
        for i in (0..blocks).filter(|&i| !color_done.get(i)) {
            out[i * block_size + color_offset + half] = next();
        }
    }
    if mirrored {
        mirror_edges(&mut out, blocks);
    }
    Ok(out)
}

fn is_edge(i: usize) -> bool {
    (1u32 << i) & 0xC000_0003 != 0
}

/// Runs over blocks not in `done`; `f` gets each block and the run's flag.
fn sub_runs(bits: &mut Bits, blocks: usize, done: &mut BlockSet, mut f: impl FnMut(usize, &mut BlockSet, bool)) {
    let mut i = 0;
    while i < blocks {
        let (mut run, flag) = bits.run();
        while run > 0 && i < blocks {
            if !done.get(i) {
                f(i, done, flag);
                run -= 1;
            }
            i += 1;
        }
        while i < blocks && done.get(i) {
            i += 1;
        }
    }
}

/// Runs for the constant-alpha passes: each run has a level (0: none,
/// 1: transparent, 2: the pass's alpha) coded as one or two bits. Blocks
/// set by a run are marked in `alpha_done`.
fn sub_alpha_runs(
    bits: &mut Bits,
    blocks: usize,
    color_done: &BlockSet,
    alpha_done: &mut BlockSet,
    mut f: impl FnMut(usize, u32),
) {
    let mut i = 0;
    while i < blocks {
        let (mut run, first) = bits.run();
        let level = if first { 1 + bits.take(1) } else { 0 };
        while run > 0 && i < blocks {
            if !color_done.get(i) {
                if level != 0 {
                    f(i, level);
                    alpha_done.set(i);
                }
                run -= 1;
            }
            i += 1;
        }
        while i < blocks && color_done.get(i) {
            i += 1;
        }
    }
}

/// The DXT colour words for a solid colour (`AtexSubCode6`).
fn solid_color_block(color: u32, dxt1: bool) -> [u32; 2] {
    let rgb = [color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF];
    // Shift and 5/6/5-bit width per channel.
    let spec = [(5u32, 3u32, 2u32), (6, 2, 4), (5, 3, 2)];
    let mut base = [0u32; 3];
    let mut frac = [0u32; 3];
    for c in 0..3 {
        let (keep, shift, back) = spec[c];
        let v = rgb[c];
        let adj = (v - (v >> keep)) >> shift;
        base[c] = adj;
        let expand = |a: u32| (a >> back) + (a << shift);
        let (lo, hi) = (expand(adj), expand(adj + 1));
        frac[c] = if hi != lo { (v * 12).wrapping_sub(lo * 12) / (hi - lo) } else { 0 };
    }
    let mut pair = [(0u32, 0u32); 3];
    for c in 0..3 {
        let (b, f) = (base[c], frac[c]);
        pair[c] = match f {
            0..2 => (b, b),
            2..6 => (b, b + 1),
            6..10 => (b + 1, b),
            _ => (b + 1, b + 1),
        };
    }
    let pack = |sel: fn((u32, u32)) -> u32| sel(pair[0]) | sel(pair[1]) << 5 | sel(pair[2]) << 11;
    let (mut c1, mut c2) = (pack(|p| p.0), pack(|p| p.1));

    let (mut score, mut count) = (0, 0);
    for c in 0..3 {
        if pair[c].0 != pair[c].1 {
            score += if pair[c].0 == base[c] { frac[c] } else { 12 - frac[c] };
            count += 1;
        }
    }
    let mut avg = (score + count / 2).checked_div(count).unwrap_or(0);
    let swap = dxt1 && (avg == 5 || avg == 6 || count == 0);
    if count == 0 && !swap {
        if c2 != 0xFFFF {
            avg = 0;
            c2 += 1;
        } else {
            avg = 12;
            c1 -= 1;
        }
    }
    if (c1 < c2) != swap {
        std::mem::swap(&mut c1, &mut c2);
        avg = 12 - avg;
    }
    let table = if swap {
        2
    } else {
        match avg {
            0..2 => 0,
            2..6 => 2,
            6..10 => 3,
            _ => 1,
        }
    };
    let index = table * 5;
    let index = (index << 4 | index) * 0x0101_0101;
    [c2 << 16 | c1, index]
}

/// `AtexSubCode7`: fill the edge blocks of a 256x256 DXT3 texture by
/// mirroring their neighbours inwards.
fn mirror_edges(out: &mut [u32], blocks: usize) {
    for i in 0..blocks {
        let (lo, hi) = (i & 0x3F, i >> 6);
        let (flip_lo, flip_hi) = (is_edge(lo & 31), is_edge(hi & 31));
        if !flip_lo && !flip_hi {
            continue;
        }
        let src = ((if flip_hi { hi ^ 3 } else { hi }) << 6) + if flip_lo { lo ^ 3 } else { lo };
        if src >= blocks {
            continue;
        }
        let [mut d0, mut d1, d2, mut d3]: [u32; 4] = out[src * 4..src * 4 + 4].try_into().unwrap();
        if flip_lo {
            // Mirror horizontally: 4-bit alpha nibbles and 2-bit indices
            // within each row.
            for _ in 0..2 {
                let t3 = ((d0 >> 8) & 0x00F0_00F0) | (d0 & 0x0F00_0F00);
                let t6 = ((d0 & 0xFFFF_000F) << 8) | (d0 & 0x00F0_00F0);
                d0 = (t3 >> 4) | (t6 << 4);
            }
            let t9 = ((d3 & 0xFF03_0303) << 4) | (d3 & 0x0C0C_0C0C);
            let t12 = ((d3 >> 4) & 0x0C0C_0C0C) | (d3 & 0x3030_3030);
            d3 = (t9 << 2) | (t12 >> 2);
        }
        if flip_hi {
            // Mirror vertically: swap rows.
            let t = d0;
            d0 = d1.rotate_left(16);
            d1 = t.rotate_left(16);
            let t3 = (d3 & 0x00FF_0000) | (d3 >> 16);
            let t6 = (d3 << 16) | (d3 & 0x0000_FF00);
            d3 = (t3 >> 8) | (t6 << 8);
        }
        out[i * 4..i * 4 + 4].copy_from_slice(&[d0, d1, d2, d3]);
    }
}

/// The 4 colours of a DXT colour block (`c0`, `c1` 5:6:5).
fn palette(c0: u16, c1: u16, four_color: bool) -> [[u8; 4]; 4] {
    // Standard 5:6:5 with red on top (GWMB decodes in BGRA order).
    let expand = |c: u16| [((c >> 11) << 3) as u8, (((c >> 5) & 0x3F) << 2) as u8, ((c & 0x1F) << 3) as u8, 255];
    let (a, b) = (expand(c0), expand(c1));
    let mix = |wa: u32, wb: u32, d: u32| -> [u8; 4] {
        std::array::from_fn(|i| if i == 3 { 255 } else { ((a[i] as u32 * wa + b[i] as u32 * wb) / d) as u8 })
    };
    if four_color { [a, b, mix(2, 1, 3), mix(1, 2, 3)] } else { [a, b, mix(1, 1, 2), [0, 0, 0, 0]] }
}

/// Write a block's colours; `alpha(k)` gives pixel `k`'s alpha, if the
/// format has its own.
fn put_block(
    out: &mut [[u8; 4]],
    width: usize,
    bx: usize,
    by: usize,
    colors: [[u8; 4]; 4],
    indices: u32,
    alpha: Option<&dyn Fn(usize) -> u8>,
) {
    for k in 0..16 {
        let mut c = colors[(indices >> (2 * k) & 3) as usize];
        if let Some(alpha) = alpha {
            c[3] = alpha(k);
        }
        out[(by * 4 + k / 4) * width + bx * 4 + k % 4] = c;
    }
}

fn decode_dxt1(words: &[u32], width: usize, height: usize) -> Vec<[u8; 4]> {
    let mut out = vec![[0u8; 4]; width * height];
    for (i, b) in words.chunks_exact(2).enumerate() {
        let (c0, c1) = (b[0] as u16, (b[0] >> 16) as u16);
        let (bx, by) = (i % (width / 4), i / (width / 4));
        put_block(&mut out, width, bx, by, palette(c0, c1, c0 > c1), b[1], None);
    }
    out
}

fn decode_dxt3(words: &[u32], width: usize, height: usize) -> Vec<[u8; 4]> {
    let mut out = vec![[0u8; 4]; width * height];
    for (i, b) in words.chunks_exact(4).enumerate() {
        let alpha = b[0] as u64 | (b[1] as u64) << 32;
        let (c0, c1) = (b[2] as u16, (b[2] >> 16) as u16);
        let (bx, by) = (i % (width / 4), i / (width / 4));
        let a = |k: usize| (((alpha >> (4 * k)) & 15) << 4) as u8;
        put_block(&mut out, width, bx, by, palette(c0, c1, true), b[3], Some(&a));
    }
    out
}

fn decode_dxt5(words: &[u32], width: usize, height: usize) -> Vec<[u8; 4]> {
    let mut out = vec![[0u8; 4]; width * height];
    for (i, b) in words.chunks_exact(4).enumerate() {
        let (a0, a1) = (b[0] & 0xFF, (b[0] >> 8) & 0xFF);
        let table = (b[0] >> 16) as u64 | (b[1] as u64) << 16;
        let mut levels = [0u8; 8];
        levels[0] = a0 as u8;
        levels[1] = a1 as u8;
        if a0 > a1 {
            for z in 0..6 {
                levels[z + 2] = (((6 - z as u32) * a0 + (z as u32 + 1) * a1) / 7) as u8;
            }
        } else {
            for z in 0..4 {
                levels[z + 2] = (((4 - z as u32) * a0 + (z as u32 + 1) * a1) / 5) as u8;
            }
            levels[6] = 0;
            levels[7] = 255;
        }
        let (c0, c1) = (b[2] as u16, (b[2] >> 16) as u16);
        let (bx, by) = (i % (width / 4), i / (width / 4));
        let a = |k: usize| levels[((table >> (3 * k)) & 7) as usize];
        put_block(&mut out, width, bx, by, palette(c0, c1, true), b[3], Some(&a));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_codes_match_client_table() {
        // byte_79053C as pairs.
        assert_eq!(RUN_CODES[0], (6, 0x10));
        assert_eq!(RUN_CODES[15], (6, 1));
        assert_eq!(RUN_CODES[16], (2, 0x11));
        assert_eq!(RUN_CODES[31], (2, 0x11));
        assert_eq!(RUN_CODES[32], (1, 0));
        assert_eq!(RUN_CODES[63], (1, 0));
    }

    /// The textures among the reference model files (`testdata/models`,
    /// which `fetch-models` fills from the props file references) decode.
    #[test]
    fn decodes_reference_textures() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join("models");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let (mut seen, mut decoded) = (0, 0);
        for entry in entries.flatten() {
            let data = std::fs::read(entry.path()).unwrap();
            if !data.starts_with(b"ATEX") && !data.starts_with(b"ATTX") {
                continue;
            }
            seen += 1;
            if let Ok(t) = decode(&data) {
                assert_eq!(t.pixels.len(), t.width * t.height);
                decoded += 1;
            }
        }
        eprintln!("{decoded}/{seen} textures decoded");
        assert!(decoded * 20 >= seen * 19, "{decoded}/{seen}");
    }

    #[test]
    fn bits_read_msb_first_across_words() {
        let words = [0x8000_0001u32, 0xF000_0000];
        let mut b = Bits { words: &words, pos: 1, current: words[0], next: 0, remaining: 0 };
        assert_eq!(b.take(1), 1);
        assert_eq!(b.take(30), 0);
        assert_eq!(b.take(1), 1);
        assert_eq!(b.take(4), 0xF);
        assert_eq!(b.take(4), 0);
    }
}
