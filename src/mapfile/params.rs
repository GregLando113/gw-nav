//! Map Parameters chunk (`0x1000000C` / `0x2000000C`, identical in both
//! stages), from the client's `Engine\Map\MapParams.cpp`.

use super::tags::Reader;
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x5943_EEEF;
pub const VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct MapParams {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
    /// Top byte: map type (0 is treated as 1). Bit 0: the map has water.
    pub flags: u32,
    /// Unknown; differs between revisions of a map. The client xors it
    /// with a hash of the file name when loading.
    pub unknown: [u8; 16],
}

impl MapParams {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(MapFileError::BadVersion(version as u32));
        }
        Ok(Self {
            min_x: r.f32()?,
            min_y: r.f32()?,
            max_x: r.f32()?,
            max_y: r.f32()?,
            flags: r.u32()?,
            unknown: r.array()?,
        })
    }

    /// The map type as the client sees it (`MapParams_load_from_buffer`
    /// turns 0 into 1). Path generation uses gentler slope limits below 2.
    pub fn map_type(&self) -> u32 {
        (self.flags >> 24).max(1)
    }

    pub fn has_water(&self) -> bool {
        self.flags & 1 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        // 0x1000000C of the current revision of map file 290943.
        let data = hex(
            "efee435902000090c60000f0c60000a8460000f0462000000401f4948b2e822640888472adf68d59b9",
        );
        let p = MapParams::parse(&data).unwrap();
        assert_eq!((p.min_x, p.min_y, p.max_x, p.max_y), (-18432.0, -30720.0, 21504.0, 30720.0));
        assert_eq!(p.flags, 0x0400_0020);
        assert_eq!(p.map_type(), 4);
        assert!(!p.has_water());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
