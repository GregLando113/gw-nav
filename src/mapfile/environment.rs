//! Environment chunk (`0x10000009`, copied unchanged into stage 2): sky,
//! fog, lighting and water settings. Layout from GuildWarsMapBrowser
//! (`FFNA_MapFile.h`, `EnvironmentInfoChunk`). Only lighting and water are
//! decoded; the other sections are skipped by their fixed record sizes.

use super::tags::Reader;
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x9299_1030;

/// Ambient and sun light (section 3). Colours are RGB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lighting {
    pub ambient: [u8; 3],
    pub ambient_intensity: u8,
    pub sun: [u8; 3],
    pub sun_intensity: u8,
}

/// Water settings (section 6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Water {
    pub mode: u8,
    pub flags: u8,
    /// Water surface height (z, which points down).
    pub surface_z: f32,
    /// RGBA.
    pub absorption: [u8; 4],
    /// RGBA.
    pub pattern: [u8; 4],
    /// Index into the environment file references (`0x11000009`).
    pub color_texture: u16,
    pub distortion_texture: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Environment {
    pub lighting: Vec<Lighting>,
    pub water: Vec<Water>,
}

impl Environment {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        r.u16()?;
        let sky_variant = r.u16()?;
        let section = |r: &mut Reader, record: usize| -> Result<usize> {
            r.u8()?;
            let n = r.u16()? as usize;
            r.bytes(n * record)?;
            Ok(n)
        };
        section(&mut r, 10)?;
        section(&mut r, 6)?;
        section(&mut r, 19)?;
        let lighting = read_records(&mut r, 8, |r| {
            let [ab, ag, ar, ai, sb, sg, sr, si] = r.array()?;
            Ok(Lighting { ambient: [ar, ag, ab], ambient_intensity: ai, sun: [sr, sg, sb], sun_intensity: si })
        })?;
        section(&mut r, 2)?;
        section(&mut r, if sky_variant > 0 { 16 } else { 15 })?;
        let water = read_records(&mut r, 57, |r| {
            let (mode, flags) = (r.u8()?, r.u8()?);
            r.bytes(3)?;
            let surface_z = r.f32()?;
            r.bytes(36)?;
            let [ab, ag, ar, aa] = r.array()?;
            let [pb, pg, pr, pa] = r.array()?;
            Ok(Water {
                mode,
                flags,
                surface_z,
                absorption: [ar, ag, ab, aa],
                pattern: [pr, pg, pb, pa],
                color_texture: r.u16()?,
                distortion_texture: r.u16()?,
            })
        })?;
        // Section 7 (4-byte records) and a tail of unknown layout follow.
        Ok(Self { lighting, water })
    }

    /// The water settings, assuming the map uses the first record. Which
    /// record is active is in the undecoded tail.
    pub fn water(&self) -> Option<&Water> {
        self.water.first()
    }
}

/// A section of fixed-size records: `u8 type, u16 count`, then the records.
fn read_records<T>(r: &mut Reader, size: usize, mut f: impl FnMut(&mut Reader) -> Result<T>) -> Result<Vec<T>> {
    r.u8()?;
    let n = r.u16()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let start = r.pos();
        out.push(f(r)?);
        debug_assert_eq!(r.pos() - start, size);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::testdata::MapPair;

    #[test]
    fn parses_samples() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0009, 0x2000_0009) else { continue };
            assert_eq!(strip, bloated, "{pair:?}: copied unchanged");
            let env = Environment::parse(&strip).unwrap();
            assert!(!env.lighting.is_empty() && !env.water.is_empty(), "{pair:?}: {env:?}");
            eprintln!("{pair:?}: {:?} {:?}", env.lighting.first(), env.water());
        }
    }
}
