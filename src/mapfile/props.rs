//! Props chunk (`0x10000004` / `0x20000004`), from the client's
//! `Engine\Map\Props\PrDataBloat.cpp` and `PrCollision.cpp`.
//!
//! Stage 2 adds the props' collision outlines, transformed into world
//! space, which path generation consumes.

use super::tags::Reader;
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x3958_3392;

/// Base file ids of the models that draw zone portals: 43045 in Prophecies
/// and Factions maps, 247212 in Nightfall and Eye of the North maps. The
/// map file has no other marker for them (checked in game, see
/// `docs/mapblob-format.md`), and a few copies are decoration.
pub const PORTAL_MODELS: &[u32] = &[43045, 247212];
pub const VERSION: u8 = 0x11;

pub mod tag {
    pub const PROPS: u8 = 0;
    pub const COLLISION: u8 = 1;
    pub const PLANE_PROPS: u8 = 2;
    pub const PORTAL_POINTS: u8 = 3;
    pub const LINKS: u8 = 4;
    pub const EXTRA: u8 = 6;
}

/// A prop placement from stage-1 tag 0 (20 bytes, then the extra points).
#[derive(Debug, Clone, PartialEq)]
pub struct PropDef {
    /// Index into the props file reference list (`0x11000004`).
    pub model: u16,
    pub position: [f32; 3],
    /// Rotation angles in 1/256 turns.
    pub rotation: [u8; 3],
    /// Scale: `byte * 1.9921875 / 256 + 0.0078125`.
    pub scale: u8,
    /// Bit 0: no collision from the model.
    pub flags: u8,
    /// Extra ground collision polygon, relative to `position`.
    pub points: Vec<[i16; 2]>,
}

/// Stage-1 props chunk, tag 0 only.
#[derive(Debug, Clone, PartialEq)]
pub struct PropsStrip {
    pub props: Vec<PropDef>,
}

impl PropsStrip {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        let version = r.u8()?;
        if version != 0x11 && version != 0x12 {
            return Err(MapFileError::BadVersion(version as u32));
        }
        r.strip_tag(tag::PROPS)?;
        let count = r.u16()?;
        let props = (0..count)
            .map(|_| {
                let model = r.u16()?;
                let position = [r.f32()?, r.f32()?, r.f32()?];
                let rotation = [r.u8()?, r.u8()?, r.u8()?];
                let (scale, flags, n) = (r.u8()?, r.u8()?, r.u8()?);
                let points = (0..n).map(|_| Ok([r.u16()? as i16, r.u16()? as i16])).collect::<Result<_>>()?;
                Ok(PropDef { model, position, rotation, scale, flags, points })
            })
            .collect::<Result<_>>()?;
        Ok(Self { props })
    }
}

/// A world-space collision outline point of a prop (24 bytes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionPoint {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Path plane (0 = ground).
    pub plane: u32,
    /// Model point flags; `0x4` is set when the edge to the next point is
    /// a portal.
    pub flags: u32,
    pub portal: u16,
    pub portal_plane: u16,
}

pub mod point_flag {
    pub const PORTAL: u32 = 0x4;
}

/// The collision data of a stage-2 props chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct PropsCollision {
    /// Tag 1: prop-plane outlines first, then ground-plane outlines.
    pub points: Vec<CollisionPoint>,
    /// Tag 2: the prop owning each path plane (also path tag 12).
    pub plane_props: Vec<u16>,
    /// Tag 3: portal edges as point pairs.
    pub portal_points: Vec<[f32; 2]>,
}

fn counted<'a>(r: &mut Reader<'a>, tag: u8, size: usize) -> Result<(u32, Reader<'a>)> {
    let mut s = Reader::new(r.tag(tag)?);
    let count = s.u32()?;
    if s.remaining().len() != count as usize * size {
        return Err(MapFileError::Invalid("props section size"));
    }
    Ok((count, s))
}

impl PropsCollision {
    /// Read the collision sections of a stage-2 props chunk.
    pub fn parse_bloated(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(MapFileError::BadVersion(version as u32));
        }
        r.tag(tag::PROPS)?;

        let (n, mut s) = counted(&mut r, tag::COLLISION, 24)?;
        let points = (0..n)
            .map(|_| {
                Ok(CollisionPoint {
                    x: s.f32()?,
                    y: s.f32()?,
                    z: s.f32()?,
                    plane: s.u32()?,
                    flags: s.u32()?,
                    portal: s.u16()?,
                    portal_plane: s.u16()?,
                })
            })
            .collect::<Result<_>>()?;

        let (n, mut s) = counted(&mut r, tag::PLANE_PROPS, 2)?;
        let plane_props = (0..n).map(|_| s.u16()).collect::<Result<_>>()?;

        let (n, mut s) = counted(&mut r, tag::PORTAL_POINTS, 8)?;
        let portal_points = (0..n).map(|_| Ok([s.f32()?, s.f32()?])).collect::<Result<_>>()?;

        Ok(Self { points, plane_props, portal_points })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::path::PathBloated;
    use crate::mapfile::testdata::MapPair;

    #[test]
    fn parses_bloated_collision() {
        for pair in MapPair::all() {
            let Some((_, props)) = pair.chunks(0x1000_0004, 0x2000_0004) else { continue };
            let Some((_, path)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let props = PropsCollision::parse_bloated(&props).unwrap();
            let path = PathBloated::parse(&path).unwrap();
            // The plane owner table is copied into path tag 12.
            assert_eq!(props.plane_props, path.plane_indices, "{pair:?}");
            assert_eq!(props.portal_points.len() % 2, 0);
            eprintln!(
                "{pair:?}: {} collision points, {} planes, {} portal points",
                props.points.len(),
                props.plane_props.len(),
                props.portal_points.len()
            );
        }
    }
}
