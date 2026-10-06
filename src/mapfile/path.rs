//! Path chunk (`0x10000008` / `0x20000008`), the pathing data.
//!
//! Stage 1 only carries the start points and a checksum; the client
//! generates the planes (tag 8), plane indices (tag 12) and obstacles
//! (tag 13) during bloat (`PathStatic_Import`).

use super::tags::{Reader, TAG_END, Writer};
use super::{MapFileError, Result, Stage};

pub const SIGNATURE: u32 = 0xEEFE_704C;
pub const VERSION: u32 = 12;

pub mod tag {
    pub const START_POINTS: u8 = 7;
    pub const PLANES: u8 = 8;
    pub const PLANE_INDICES: u8 = 12;
    pub const OBSTACLES: u8 = 13;
    pub const CHECKSUM: u8 = 14;
}

/// Read `u32 signature, version, u32 sequence`, where the version is a `u8`
/// in stage 1 and a `u32` in stage 2. Returns the sequence.
fn read_header(r: &mut Reader, stage: Stage) -> Result<u32> {
    let signature = r.u32()?;
    if signature != SIGNATURE {
        return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
    }
    let version = match stage {
        Stage::Strip => r.u8()? as u32,
        Stage::Bloated => r.u32()?,
    };
    if version != VERSION {
        return Err(MapFileError::BadVersion(version));
    }
    r.u32()
}

/// `u16 count, count * vec2f`.
fn read_points(r: &mut Reader) -> Result<Vec<[f32; 2]>> {
    let count = r.u16()?;
    (0..count).map(|_| Ok([r.f32()?, r.f32()?])).collect()
}

/// Stage-1 path chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct PathStrip {
    pub sequence: u32,
    /// Tag 7. Copied unchanged into stage 2.
    pub start_points: Vec<[f32; 2]>,
    /// Tag 14. The client xors the CRCs of the tag 8 and tag 13 payloads it
    /// generates and records in stage 2 whether they differ from this.
    pub checksum: u32,
}

impl PathStrip {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let sequence = read_header(&mut r, Stage::Strip)?;
        r.strip_tag(tag::START_POINTS)?;
        let start_points = read_points(&mut r)?;
        r.strip_tag(tag::CHECKSUM)?;
        let checksum = r.u32()?;
        r.u8()?; // Skipped by the client; 0 in every sample.
        r.strip_tag(TAG_END)?;
        Ok(Self { sequence, start_points, checksum })
    }
}

/// Stage-2 path chunk, split into its top-level sections. The plane data
/// is not decoded yet.
#[derive(Debug, Clone, PartialEq)]
pub struct PathBloated<'a> {
    pub sequence: u32,
    pub start_points: Vec<[f32; 2]>,
    pub planes: &'a [u8],
    pub plane_indices: Vec<u16>,
    pub obstacles: &'a [u8],
    pub checksum: u32,
    /// Whether the client's generated data did not match `checksum`.
    pub checksum_mismatch: bool,
}

impl<'a> PathBloated<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let sequence = read_header(&mut r, Stage::Bloated)?;
        let start_points = read_points(&mut Reader::new(r.tag(tag::START_POINTS)?))?;
        let planes = r.tag(tag::PLANES)?;
        let plane_indices = {
            let mut s = Reader::new(r.tag(tag::PLANE_INDICES)?);
            let count = s.u16()?;
            (0..count).map(|_| s.u16()).collect::<Result<_>>()?
        };
        let obstacles = r.tag(tag::OBSTACLES)?;
        let mut s = Reader::new(r.tag(tag::CHECKSUM)?);
        let checksum = s.u32()?;
        let checksum_mismatch = s.u8()? != 0;
        r.tag(TAG_END)?;
        Ok(Self {
            sequence,
            start_points,
            planes,
            plane_indices,
            obstacles,
            checksum,
            checksum_mismatch,
        })
    }
}

/// `Crc_Compute`: CRC-32 (IEEE, reflected polynomial `0xEDB88320`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Write a stage-2 path chunk (the bloat writers from `PathChunk_ReadHeader`
/// to `PathChunk_SkipTerminator`). `planes` and `obstacles` are the tag 8
/// and tag 13 payloads; the mismatch flag is computed from them.
pub fn write_bloated(
    strip: &PathStrip,
    planes: &[u8],
    plane_indices: &[u16],
    obstacles: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(SIGNATURE).u32(VERSION).u32(strip.sequence);

    let open = w.begin_tag(tag::START_POINTS);
    w.u16(strip.start_points.len() as u16);
    for p in &strip.start_points {
        w.f32(p[0]).f32(p[1]);
    }
    w.end_tag(open);

    w.tag(tag::PLANES, planes);

    let open = w.begin_tag(tag::PLANE_INDICES);
    w.u16(plane_indices.len() as u16);
    for &i in plane_indices {
        w.u16(i);
    }
    w.end_tag(open);

    w.tag(tag::OBSTACLES, obstacles);

    let computed = crc32(planes) ^ crc32(obstacles);
    let open = w.begin_tag(tag::CHECKSUM);
    w.u32(strip.checksum).u8((computed != strip.checksum) as u8);
    w.end_tag(open);

    w.tag(TAG_END, &[]);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::testdata::MapPair;

    #[test]
    fn parses_strip() {
        // 0x10000008 of an old revision of map file 290943.
        let data = [
            0x4c, 0x70, 0xfe, 0xee, 0x0c, 0x71, 0, 0, 0, //
            7, 1, 0, 0, 0, 0x80, 0x3f, 0, 0, 0, 0x40, //
            14, 0x4a, 0xda, 0x92, 0xea, 0, //
            0xff,
        ];
        let path = PathStrip::parse(&data).unwrap();
        assert_eq!(path.sequence, 0x71);
        assert_eq!(path.start_points, vec![[1.0, 2.0]]);
        assert_eq!(path.checksum, 0xEA92DA4A);
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Rebuilding the client's chunk from its parts reproduces it, including
    /// the mismatch flag.
    #[test]
    fn write_bloated_roundtrips_client() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let strip = PathStrip::parse(&strip).unwrap();
            let path = PathBloated::parse(&bloated).unwrap();
            let ours = write_bloated(&strip, path.planes, &path.plane_indices, path.obstacles);
            assert!(ours == bloated, "{pair:?}");
        }
    }

    #[test]
    fn strip_matches_bloated_oracle() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let strip = PathStrip::parse(&strip).unwrap();
            let bloated = PathBloated::parse(&bloated).unwrap();
            assert_eq!(strip.sequence, bloated.sequence, "{pair:?}");
            assert_eq!(strip.start_points, bloated.start_points, "{pair:?}");
            assert_eq!(strip.checksum, bloated.checksum, "{pair:?}");
        }
    }
}
