//! The mission chunk (`0x10000007`, copied unchanged into stage 2): named
//! points on the map, mostly spawn points.
//!
//! ```text
//! u32 0x40010020, u8 version (10), 6 × f32 (not decoded)
//! 2 × { u16 n, n × { i32 x, i32 y, u8 facing, u32 tag } }
//! (not decoded)
//! ```
//!
//! A tag is a C multi-character constant such as `'vale'`, so its bytes
//! are stored reversed; 4-digit tags are map ids. Observed in game:
//! arriving by map travel puts the character 75 units from one of the
//! points tagged with the map's own id, facing `facing / 256` of a turn.
//! The other tags look like arrivals from particular neighbouring maps,
//! placed near the portal back to them (see `docs/mapblob-format.md`).

use serde::{Deserialize, Serialize};

use super::tags::Reader;
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x4001_0020;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionPoint {
    /// Which of the two lists it came from (0 or 1).
    pub list: u8,
    pub x: f32,
    pub y: f32,
    /// Facing in 256ths of a turn, counter-clockwise from +x.
    pub facing: u8,
    /// The tag as written in source (`vale`, `0546`), empty if none.
    pub tag: String,
}

impl MissionPoint {
    pub fn facing_radians(&self) -> f32 {
        self.facing as f32 * std::f32::consts::TAU / 256.0
    }

    /// The map id of a 4-digit tag.
    pub fn map_id(&self) -> Option<u32> {
        (self.tag.len() == 4 && self.tag.bytes().all(|b| b.is_ascii_digit())).then(|| self.tag.parse().ok())?
    }
}

/// Decode a tag: reverse the bytes, drop NULs, show other unprintable
/// bytes as `?`.
fn tag(bytes: [u8; 4]) -> String {
    bytes
        .iter()
        .rev()
        .filter(|&&b| b != 0)
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

/// The points of a mission chunk payload.
pub fn parse(data: &[u8]) -> Result<Vec<MissionPoint>> {
    let mut r = Reader::new(data);
    let signature = r.u32()?;
    if signature != SIGNATURE {
        return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
    }
    r.u8()?;
    r.bytes(24)?;
    let mut points = Vec::new();
    for list in 0..2 {
        let n = r.u16()?;
        for _ in 0..n {
            let x = i32::from_le_bytes(r.array()?);
            let y = i32::from_le_bytes(r.array()?);
            let facing = r.u8()?;
            points.push(MissionPoint { list, x: x as f32, y: y as f32, facing, tag: tag(r.array()?) });
        }
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::Ffna;
    use crate::pathgen::testing::Fixture;

    #[test]
    fn decodes_points() {
        let mut data = vec![0x20, 0x00, 0x01, 0x40, 10];
        data.extend([0; 24]);
        data.extend(1u16.to_le_bytes());
        data.extend((-12024i32).to_le_bytes());
        data.extend((-23710i32).to_le_bytes());
        data.push(64);
        data.extend(*b"1po\0");
        data.extend(2u16.to_le_bytes());
        for (x, tag) in [(5i32, *b"6450"), (6, [0; 4])] {
            data.extend(x.to_le_bytes());
            data.extend(7i32.to_le_bytes());
            data.push(0);
            data.extend(tag);
        }
        data.extend([1, 2, 3]);
        let points = parse(&data).unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(
            points[0],
            MissionPoint { list: 0, x: -12024.0, y: -23710.0, facing: 64, tag: "op1".into() }
        );
        assert!((points[0].facing_radians() - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert_eq!((points[1].list, points[1].tag.as_str(), points[1].map_id()), (1, "0546", Some(546)));
        assert_eq!((points[2].tag.as_str(), points[2].map_id()), ("", None));
        assert!(parse(&data[1..]).is_err());
    }

    /// Jaga Moraine's own spawn point and its `op1`.
    #[test]
    fn sample_map() {
        let Some(f) = Fixture::all().find(|f| f.pair.base_id == 290943) else { return };
        let strip = f.pair.strip().unwrap();
        let points = parse(Ffna::parse(&strip).unwrap().chunk(0x1000_0007).unwrap()).unwrap();
        assert_eq!(points.len(), 11);
        assert!(points.contains(&MissionPoint { list: 0, x: 4857.0, y: 5439.0, facing: 116, tag: "0546".into() }));
        assert!(points.iter().any(|p| p.tag == "op1" && (p.x, p.y) == (-12024.0, -23710.0)));
    }
}
