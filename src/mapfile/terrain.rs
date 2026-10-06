//! Terrain chunk (`0x10000002` / `0x20000002`), from the client's
//! `Engine\Map\Terrain\TrnDataBloat.cpp` and `TrnCodecHeight.cpp`.

use super::bits::BitReader;
use super::huffman::Huffman;
use super::tags::{Reader, TAG_END};
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x8782_1134;
pub const VERSION: u32 = 0x11;
/// World units between height samples (`XY_DIST`).
pub const XY_DIST: f32 = 96.0;
/// Heights are coded in square chunks of this many samples per side.
pub const CHUNK_SIZE: usize = 32;

pub mod tag {
    pub const HEADER: u8 = 0;
    pub const HEIGHTS: u8 = 1;
    pub const TEXTURES: u8 = 2;
    pub const WATER: u8 = 3;
    pub const INDEX_ARRAY: u8 = 4;
    pub const DATA_ARRAY: u8 = 5;
    pub const CHUNK_GRID: u8 = 7;
    /// Optional, after the chunk grid: the shiny terrain settings
    /// ([`super::TerrainShiny`]). The client reuses tag 3 for it.
    pub const SHINY: u8 = 3;
    /// Stage 2 only: generated per-sample lighting.
    pub const LIGHTING: u8 = 9;
}

/// Bytes of shadow data per chunk in the chunk grid (tag 7).
pub const CHUNK_SHADOW_SIZE: usize = 0x80;

/// Stage-1 tag 0.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainHeader {
    /// Height samples along x and y; both multiples of [`CHUNK_SIZE`].
    pub dims: [usize; 2],
    /// Low 6 bits of the packed header times 3072. Meaning unknown.
    pub unknown_scale: f32,
    /// Stored as a byte; the client converts it with `* 282.74335 / 45720`.
    pub angle: u8,
    pub unknown_u16: u16,
    /// Two bytes the client stores in stage 2 as `n * 2 / 255`.
    pub unknown_bytes: [u8; 2],
}

impl TerrainHeader {
    fn read(r: &mut Reader) -> Result<Self> {
        r.strip_tag(tag::HEADER)?;
        let packed = r.u32()?;
        if packed & 0xC0 != 0x80 || (packed >> 8) & 0xFF != XY_DIST as u32 {
            return Err(MapFileError::Invalid("terrain header"));
        }
        let dim_y = (((packed >> 16) + 1) & 0xFF) as usize * CHUNK_SIZE;
        let dim_x = ((packed >> 24) + 1) as usize * CHUNK_SIZE;
        Ok(Self {
            dims: [dim_x, dim_y],
            unknown_scale: (packed & 0x3F) as f32 * 3072.0,
            angle: r.u8()?,
            unknown_u16: r.u16()?,
            unknown_bytes: [r.u8()?, r.u8()?],
        })
    }
}

/// Stage-1 terrain chunk: the header and heights, which is what pathing
/// needs. [`TerrainSurface`] has the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainStrip {
    pub header: TerrainHeader,
    /// `dims[0] * dims[1]` heights in chunk-major order: 32x32 chunks, row
    /// by row, each chunk row-major. This is also the stage-2 tag 1 layout.
    pub heights: Vec<f32>,
}

impl TerrainStrip {
    pub fn parse(data: &[u8]) -> Result<Self> {
        Self::read(&mut Reader::new(data))
    }

    /// Parse the whole chunk, including the surface tags after the heights.
    pub fn parse_with_surface(data: &[u8]) -> Result<(Self, TerrainSurface)> {
        let mut r = Reader::new(data);
        let strip = Self::read(&mut r)?;
        let surface = TerrainSurface::read(&mut r, strip.header.dims)?;
        Ok((strip, surface))
    }

    fn read(r: &mut Reader) -> Result<Self> {
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        let version = r.u8()? as u32;
        if version != VERSION {
            return Err(MapFileError::BadVersion(version));
        }
        let header = TerrainHeader::read(r)?;
        r.strip_tag(tag::HEIGHTS)?;
        let (heights, used) = decode_heights(header.dims, r.remaining())?;
        r.bytes(used)?;
        Ok(Self { header, heights })
    }

    /// Height at sample `(x, y)`.
    pub fn height(&self, x: usize, y: usize) -> f32 {
        let chunks_x = self.header.dims[0] / CHUNK_SIZE;
        let chunk = (y / CHUNK_SIZE) * chunks_x + x / CHUNK_SIZE;
        self.heights[chunk * CHUNK_SIZE * CHUNK_SIZE + (y % CHUNK_SIZE) * CHUNK_SIZE + x % CHUNK_SIZE]
    }
}

/// One entry of the chunk grid (tag 7), per 32x32 chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkGridEntry {
    /// Variable-length per-chunk data. Meaning unknown; the client keeps it
    /// as `m_shadowData`.
    pub data: Vec<u8>,
    pub shadow: [u8; CHUNK_SHADOW_SIZE],
}

/// The terrain tags after the heights: what rendering needs. Stage 1 stores
/// all of these as stage 2 does except tags 4 and 5, which are bit-packed.
/// Tag 9 (lighting) exists only in stage 2.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainSurface {
    /// Tag 2: a terrain texture index per sample, in the same chunk-major
    /// order as the heights.
    pub textures: Vec<u8>,
    /// Tag 4. The client masks the values with `0x7F` (`TerrainBuild_SetIndexArray`).
    pub index_array: Vec<u8>,
    /// Tag 5 (`TerrainBuild_SetDataArray`).
    pub data_array: Vec<u8>,
    /// Tag 3: water mask, 2 bits per sample (`dims[0] * dims[1] / 4` bytes).
    pub water: Vec<u8>,
    /// Tag 7: one entry per chunk, chunk rows top to bottom.
    pub chunk_grid: Vec<ChunkGridEntry>,
    /// The optional tag 3 after the chunk grid.
    pub shiny: Option<TerrainShiny>,
}

/// The 17-byte tag 3 some maps have after the chunk grid, copied to stage 2
/// unchanged (`Terrain_bloat_convert_chunk_info`). The client's stage-2
/// reader (`TerrainChunk_ReadShinyConfig`) passes the floats to
/// `TerrainShiny_Configure` in the order `values[3], values[2], values[0],
/// values[1]`. Their meaning is unknown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainShiny {
    /// The first byte, which the client doesn't read.
    pub unknown: u8,
    pub values: [f32; 4],
}

impl TerrainShiny {
    pub const SIZE: usize = 17;

    fn read(r: &mut Reader) -> Result<Self> {
        Ok(Self { unknown: r.u8()?, values: [r.f32()?, r.f32()?, r.f32()?, r.f32()?] })
    }
}

impl TerrainSurface {
    fn read(r: &mut Reader, dims: [usize; 2]) -> Result<Self> {
        let samples = dims[0] * dims[1];
        r.strip_tag(tag::TEXTURES)?;
        let textures = r.bytes(samples)?.to_vec();
        r.strip_tag(tag::INDEX_ARRAY)?;
        let index_array = read_packed_bytes(r)?;
        r.strip_tag(tag::DATA_ARRAY)?;
        let data_array = read_packed_bytes(r)?;
        r.strip_tag(tag::WATER)?;
        let water = r.bytes(samples / 4)?.to_vec();
        r.strip_tag(tag::CHUNK_GRID)?;
        let chunks = (dims[0] / CHUNK_SIZE) * (dims[1] / CHUNK_SIZE);
        let chunk_grid = (0..chunks)
            .map(|_| {
                let n = r.u32()? as usize;
                let data = r.bytes(n)?.to_vec();
                Ok(ChunkGridEntry { data, shadow: r.array()? })
            })
            .collect::<Result<_>>()?;
        let shiny = match r.peek_u8() {
            Some(tag::SHINY) => {
                r.strip_tag(tag::SHINY)?;
                Some(TerrainShiny::read(r)?)
            }
            _ => None,
        };
        r.strip_tag(TAG_END)?;
        Ok(Self { textures, index_array, data_array, water, chunk_grid, shiny })
    }

    /// The stage-2 tag 7 payload, which the client writes back unchanged.
    pub fn chunk_grid_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.chunk_grid {
            out.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&entry.data);
            out.extend_from_slice(&entry.shadow);
        }
        out
    }
}

/// A `TrnBitStore`-packed byte array (stage-1 tags 4 and 5; the readers at
/// `0x7599c0` and `0x759790`): `count` in 8 bits, then, if it is nonzero, a
/// 3-bit value width and `count` values of that width, padded to a byte.
fn read_packed_bytes(r: &mut Reader) -> Result<Vec<u8>> {
    let available = r.remaining().len();
    let mut br = BitReader::new(r.remaining());
    let count = br.read(8) as usize;
    if count == 0 {
        r.bytes(1)?;
        return Ok(Vec::new());
    }
    let width = br.read(3);
    // The client's size check, and its `bitCount` assert.
    if width == 0 || (width as usize * count + 0x12) >> 3 > available {
        return Err(MapFileError::Invalid("terrain packed array"));
    }
    let values = (0..count).map(|_| br.read(width) as u8).collect();
    br.align();
    r.bytes(br.bytes_consumed())?;
    Ok(values)
}

/// Symbol meaning "raw value follows".
const ESCAPE: u32 = 0x3FF;
const ALPHABET: u32 = 0x400;
const MAX_CODE_LEN: usize = 18;

/// A coded value range: `base + read(bits)`.
#[derive(Clone, Copy)]
struct Range {
    base: i32,
    bits: u32,
}

impl Range {
    fn read_header(br: &mut BitReader) -> Self {
        let base = br.read(16) as u16 as i16 as i32;
        let bits = br.read(4) + 1;
        Self { base, bits }
    }

    fn read(self, br: &mut BitReader) -> i32 {
        self.base.wrapping_add(br.read(self.bits) as i32)
    }
}

/// Decode `TrnCodecHeight`. Returns the heights (chunk-major) and the number
/// of bytes used.
pub fn decode_heights(dims: [usize; 2], data: &[u8]) -> Result<(Vec<f32>, usize)> {
    let chunks = (dims[0] / CHUNK_SIZE) * (dims[1] / CHUNK_SIZE);
    let mut heights = vec![0f32; dims[0] * dims[1]];
    let mut br = BitReader::new(data);
    for chunk in heights.chunks_exact_mut(CHUNK_SIZE * CHUNK_SIZE).take(chunks) {
        decode_chunk(&mut br, chunk)?;
    }
    br.align();
    Ok((heights, br.bytes_consumed()))
}

/// One 32x32 chunk: two value ranges, a Huffman table, then 8x8 blocks of
/// 4x4 samples.
fn decode_chunk(br: &mut BitReader, out: &mut [f32]) -> Result<()> {
    let dc = Range::read_header(br);
    let escape = Range::read_header(br);
    br.align();
    let table = Huffman::read(br, ALPHABET, MAX_CODE_LEN).ok_or(MapFileError::Invalid("height table"))?;
    for block_y in 0..8 {
        for block_x in 0..8 {
            let origin = block_y * 4 * CHUNK_SIZE + block_x * 4;
            decode_block(br, &table, dc, escape, &mut out[origin..])?;
        }
    }
    br.align();
    Ok(())
}

/// One 4x4 block: a DC value and 15 Huffman-coded coefficients, then an
/// integer inverse transform.
fn decode_block(br: &mut BitReader, table: &Huffman, dc: Range, escape: Range, out: &mut [f32]) -> Result<()> {
    let mut c = [0i32; 16];
    if br.remaining() == 0 {
        return Err(MapFileError::Invalid("height block"));
    }
    c[0] = dc.read(br);
    for v in &mut c[1..] {
        if br.remaining() == 0 {
            return Err(MapFileError::Invalid("height block"));
        }
        let symbol = table.decode(br);
        *v = if symbol == ESCAPE { escape.read(br) } else { symbol as i32 - 0x200 };
    }

    // Columns, then rows. The client stores these in two scratch arrays.
    let mut t = [0i32; 16];
    for j in 0..4 {
        let [a, b, cc, d] = [c[j], c[4 + j], c[8 + j], c[12 + j]];
        let (diff, sum) = (a.wrapping_sub(b), a.wrapping_add(b));
        t[j] = diff.wrapping_sub(cc);
        t[4 + j] = cc.wrapping_add(diff);
        t[8 + j] = sum.wrapping_sub(d);
        t[12 + j] = d.wrapping_add(sum);
    }
    for row in 0..4 {
        let [a, b, cc, d] = [t[4 * row], t[4 * row + 1], t[4 * row + 2], t[4 * row + 3]];
        let (diff, sum) = (a.wrapping_sub(b), a.wrapping_add(b));
        let values = [diff.wrapping_sub(cc), cc.wrapping_add(diff), sum.wrapping_sub(d), d.wrapping_add(sum)];
        for (col, v) in values.into_iter().enumerate() {
            out[row * CHUNK_SIZE + col] = v as f32;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::testdata::MapPair;

    #[test]
    fn heights_match_bloated_oracle() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0002, 0x2000_0002) else { continue };
            let terrain = TerrainStrip::parse(&strip).unwrap();

            let mut r = Reader::new(&bloated);
            assert_eq!(r.u32().unwrap(), SIGNATURE);
            assert_eq!(r.u32().unwrap(), VERSION);
            let header = r.tag(tag::HEADER).unwrap();
            let dims = [
                u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize,
                u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize,
            ];
            assert_eq!(dims, terrain.header.dims, "{pair:?}");
            let expected: Vec<f32> = r
                .tag(tag::HEIGHTS)
                .unwrap()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            assert_eq!(expected.len(), terrain.heights.len(), "{pair:?}");
            let mismatches = expected.iter().zip(&terrain.heights).filter(|(a, b)| a.to_bits() != b.to_bits()).count();
            assert_eq!(mismatches, 0, "{pair:?}: {mismatches} heights differ");
            let (min, max) = terrain.heights.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &h| (lo.min(h), hi.max(h)));
            eprintln!("{pair:?}: {} heights match, range {min}..{max}", expected.len());
        }
    }

    #[test]
    fn surface_matches_bloated_oracle() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0002, 0x2000_0002) else { continue };
            let (_, surface) = TerrainStrip::parse_with_surface(&strip).unwrap();

            let mut r = Reader::new(&bloated);
            assert_eq!(r.u32().unwrap(), SIGNATURE);
            assert_eq!(r.u32().unwrap(), VERSION);
            r.tag(tag::HEADER).unwrap();
            r.tag(tag::HEIGHTS).unwrap();
            assert_eq!(r.tag(tag::TEXTURES).unwrap(), surface.textures, "{pair:?}: textures");
            let counted = |v: &[u8]| [&[v.len() as u8][..], v].concat();
            assert_eq!(r.tag(tag::INDEX_ARRAY).unwrap(), counted(&surface.index_array), "{pair:?}: tag 4");
            assert_eq!(r.tag(tag::DATA_ARRAY).unwrap(), counted(&surface.data_array), "{pair:?}: tag 5");
            assert_eq!(r.tag(tag::WATER).unwrap(), surface.water, "{pair:?}: water");
            r.tag(tag::LIGHTING).unwrap();
            assert_eq!(r.tag(tag::CHUNK_GRID).unwrap(), surface.chunk_grid_bytes(), "{pair:?}: chunk grid");
            let distinct = surface.textures.iter().collect::<std::collections::BTreeSet<_>>().len();
            eprintln!(
                "{pair:?}: surface matches; {distinct} texture indices, index array {:?}",
                surface.index_array
            );
        }
    }

    /// The tags after the heights for one 32x32 chunk, with or without the
    /// optional shiny tag (map file 190134 has one).
    #[test]
    fn reads_optional_shiny_tag() {
        let surface_bytes = |shiny: Option<&[u8]>| {
            let samples = CHUNK_SIZE * CHUNK_SIZE;
            let mut b = vec![tag::TEXTURES];
            b.extend(vec![1; samples]);
            // Tags 4 and 5: empty packed arrays.
            b.extend([tag::INDEX_ARRAY, 0, tag::DATA_ARRAY, 0, tag::WATER]);
            b.extend(vec![0; samples / 4]);
            b.extend([tag::CHUNK_GRID, 0, 0, 0, 0]);
            b.extend([0; CHUNK_SHADOW_SIZE]);
            if let Some(shiny) = shiny {
                b.push(tag::SHINY);
                b.extend(shiny);
            }
            b.push(TAG_END);
            b
        };
        let read = |b: &[u8]| TerrainSurface::read(&mut Reader::new(b), [CHUNK_SIZE, CHUNK_SIZE]);

        assert_eq!(read(&surface_bytes(None)).unwrap().shiny, None);
        // The bytes from map file 190134 (revision 380831).
        let shiny = [1, 0xd3, 0x4d, 0x82, 0x3e, 0xdd, 0xcc, 0x37, 0x45, 0x4e, 0x62, 0xb2, 0x44, 0x9c, 0xc4, 0xa0, 0x3e];
        let surface = read(&surface_bytes(Some(&shiny))).unwrap();
        let TerrainShiny { unknown, values } = surface.shiny.unwrap();
        assert_eq!(unknown, 1);
        assert_eq!(values.map(|v| (v * 100.0).round() / 100.0), [0.25, 2940.8, 1427.07, 0.31]);
        // Anything but the end tag after it is still an error.
        let mut bad = surface_bytes(Some(&shiny));
        *bad.last_mut().unwrap() = 0x08;
        assert!(read(&bad).is_err());
    }
}
