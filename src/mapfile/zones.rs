//! Zones chunk, stage 1 (`0x10000003`): the procedural foliage zones, from
//! the client's `ZnZonePop.cpp` / `ZoneData_*`. See `docs/mapblob-format.md`
//! section 7b.1.
//!
//! ```text
//! u32 0x59220320, u8 version (10)
//! sections: u8 tag, u32 len, payload; tag 0xFF (len 0) ends the chunk
//!   1: zone defs     2: prop files     3: zones
//! ```
//!
//! The defs don't name their models: they consume the zones file-reference
//! list (`0x11000003`) in order, so the models of def `i` start after the
//! models of defs `0..i`.
//!
//! The `.ini` paths follow the developers' folder tree,
//! `Chapter<N>\Missions\<Region>\<Area>\Zones\<Def>.ini`, or
//! `Missions\GuildWars\<Area>\Zones\<Def>.ini` in Prophecies maps. Defs are
//! shared between maps, and one map can use defs from several folders.

use serde::{Deserialize, Serialize};

use super::tags::{Reader, TAG_END};
use super::{MapFileError, Result};

pub const SIGNATURE: u32 = 0x5922_0320;

pub mod tag {
    pub const DEFS: u8 = 1;
    pub const PROP_FILES: u8 = 2;
    pub const ZONES: u8 = 3;
}

/// A zone def (tag 1): the layers of objects a zone is populated with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneDef {
    pub id: u32,
    /// The editor's `.ini` path; stage 2 drops it.
    pub ini_path: String,
    pub layers: Vec<ZoneLayer>,
    /// The def's models, layer by layer (`ZoneLayer::model_count` each).
    pub models: Vec<ZoneModel>,
}

impl ZoneDef {
    /// The models of each layer.
    pub fn layer_models(&self) -> impl Iterator<Item = (&ZoneLayer, &[ZoneModel])> {
        let mut start = 0;
        self.layers.iter().map(move |layer| {
            let end = (start + layer.model_count as usize).min(self.models.len());
            let models = &self.models[start.min(end)..end];
            start = end;
            (layer, models)
        })
    }
}

/// One layer of a zone def. The file stores each field as an array over
/// the layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneLayer {
    /// 0, 1 or 2 so far; meaning unknown.
    pub kind: u32,
    pub spacing: f32,
    pub collision_radius: f32,
    pub density: f32,
    /// At most 0.5 so far.
    pub scale_variance: f32,
    /// Placement pattern (0–4): random, noise table, or constant.
    pub pattern: u8,
    pub model_count: u32,
}

impl ZoneLayer {
    /// The mip level the layer is populated at (`ZoneDef_Create`): the
    /// smallest `l` in 0..4 with `spacing < 2^l * 96 * 0.1`, else 4.
    pub fn level(&self) -> u32 {
        (0..4).find(|&l| self.spacing < (1 << l) as f32 * 96.0 * 0.1).unwrap_or(4)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZoneModel {
    /// Cumulative within the layer, ending at 1.0 (or 0.9999999).
    pub cumulative_probability: f32,
    /// `0x800`: scale `u = rand` instead of `2·rand`; `8`/`0x10`: slope
    /// tests. The rest are unknown.
    pub flags: u32,
}

/// Tag 2: the models drawn as props (rendering only). On the samples these
/// are the models of the level-0 layers, deduplicated by file id.
#[derive(Debug, Clone, PartialEq)]
pub struct PropFiles {
    /// Indices into the zones file-reference list.
    pub models: Vec<u32>,
    /// Texture atlas sizes, one nibble per model, low nibble first; the
    /// meaning of the values (`0x9`, `0xa`) is unknown.
    pub atlas: Vec<u8>,
}

/// A zone polygon (tag 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub def_id: u32,
    pub flags: u8,
    /// Encoded height; see [`Zone::height`].
    pub height_raw: u16,
    pub vertices: Vec<[f32; 2]>,
}

impl Zone {
    pub fn height(&self) -> f32 {
        self.height_raw as f32 * 0.762_939_45 - 25000.0
    }

    /// Shoelace area; positive for counter-clockwise vertices.
    pub fn signed_area(&self) -> f32 {
        let v = &self.vertices;
        let sum: f32 = (0..v.len())
            .map(|i| {
                let (a, b) = (v[i], v[(i + 1) % v.len()]);
                a[0] * b[1] - b[0] * a[1]
            })
            .sum();
        sum / 2.0
    }

    /// `(min, max)` corners, or `None` without vertices.
    pub fn bounds(&self) -> Option<([f32; 2], [f32; 2])> {
        let first = *self.vertices.first()?;
        Some(self.vertices.iter().fold((first, first), |(lo, hi), p| {
            ([lo[0].min(p[0]), lo[1].min(p[1])], [hi[0].max(p[0]), hi[1].max(p[1])])
        }))
    }

    /// Whether `p` is inside the polygon (even-odd rule).
    pub fn contains(&self, [x, y]: [f32; 2]) -> bool {
        let v = &self.vertices;
        let mut inside = false;
        for i in 0..v.len() {
            let (a, b) = (v[i], v[(i + 1) % v.len()]);
            if (a[1] > y) != (b[1] > y) && x < a[0] + (y - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) {
                inside = !inside;
            }
        }
        inside
    }

    /// Area centroid, or the vertex mean for a degenerate polygon.
    pub fn centroid(&self) -> [f32; 2] {
        let v = &self.vertices;
        let area = self.signed_area();
        if area.abs() < 1e-3 {
            let n = v.len().max(1) as f32;
            let sum = v.iter().fold([0.0, 0.0], |s, p| [s[0] + p[0], s[1] + p[1]]);
            return [sum[0] / n, sum[1] / n];
        }
        let mut c = [0.0, 0.0];
        for i in 0..v.len() {
            let (a, b) = (v[i], v[(i + 1) % v.len()]);
            let cross = a[0] * b[1] - b[0] * a[1];
            c = [c[0] + (a[0] + b[0]) * cross, c[1] + (a[1] + b[1]) * cross];
        }
        [c[0] / (6.0 * area), c[1] / (6.0 * area)]
    }
}

/// The folder of a def's `.ini` path, without a trailing `\Zones`:
/// `Chapter3\Missions\Nightmare\Town`.
pub fn ini_folder(path: &str) -> &str {
    let folder = path.rsplit_once('\\').map_or("", |(folder, _)| folder);
    folder.strip_suffix("\\Zones").unwrap_or(folder)
}

/// The file name of a def's `.ini` path without the extension:
/// `NightmareTownCreepy`.
pub fn ini_stem(path: &str) -> &str {
    let name = path.rsplit_once('\\').map_or(path, |(_, name)| name);
    name.strip_suffix(".ini").or_else(|| name.strip_suffix(".INI")).unwrap_or(name)
}

/// [`ini_folder`] after its `Missions\` part, if any: `Nightmare\Town`, or
/// `GuildWars\CoastalTemple` for Prophecies paths.
pub fn short_folder(path: &str) -> &str {
    let folder = ini_folder(path);
    match folder.strip_prefix("Missions\\") {
        Some(rest) => rest,
        None => folder.split_once("\\Missions\\").map_or(folder, |(_, rest)| rest),
    }
}

/// Where a section sits in the chunk payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub tag: u8,
    /// Offset of the tag byte.
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZonesStrip {
    pub version: u8,
    /// Every section in file order, including the end tag.
    pub sections: Vec<Section>,
    pub defs: Vec<ZoneDef>,
    pub prop_files: Option<PropFiles>,
    pub zones: Vec<Zone>,
    /// Sections with other tags: `(tag, payload)`.
    pub unknown: Vec<(u8, Vec<u8>)>,
}

impl ZonesStrip {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let signature = r.u32()?;
        if signature != SIGNATURE {
            return Err(MapFileError::BadSignature { expected: SIGNATURE, found: signature });
        }
        let mut out = Self {
            version: r.u8()?,
            sections: Vec::new(),
            defs: Vec::new(),
            prop_files: None,
            zones: Vec::new(),
            unknown: Vec::new(),
        };
        loop {
            let offset = r.pos();
            let (tag, payload) = r.any_tag()?;
            out.sections.push(Section { tag, offset, len: payload.len() });
            match tag {
                TAG_END => break,
                tag::DEFS => out.defs = parse_all(payload, parse_defs)?,
                tag::PROP_FILES => out.prop_files = Some(parse_prop_files(payload)?),
                tag::ZONES => out.zones = parse_all(payload, parse_zones)?,
                _ => out.unknown.push((tag, payload.to_vec())),
            }
        }
        Ok(out)
    }

    /// Index of the first model of each def in the zones file-reference
    /// list, plus the total at the end.
    pub fn model_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for def in &self.defs {
            starts.push(starts.last().unwrap() + def.models.len());
        }
        starts
    }
}

/// Parse a whole section; leftover bytes mean the layout is wrong.
fn parse_all<T>(data: &[u8], f: impl FnOnce(&mut Reader) -> Result<T>) -> Result<T> {
    let mut r = Reader::new(data);
    let out = f(&mut r)?;
    if !r.is_empty() {
        return Err(MapFileError::Invalid("zones section has trailing bytes"));
    }
    Ok(out)
}

fn utf16z(r: &mut Reader) -> Result<String> {
    let mut units = Vec::new();
    loop {
        match r.u16()? {
            0 => return Ok(String::from_utf16_lossy(&units)),
            u => units.push(u),
        }
    }
}

fn array<'a, T>(r: &mut Reader<'a>, n: u32, mut f: impl FnMut(&mut Reader<'a>) -> Result<T>) -> Result<Vec<T>> {
    (0..n).map(|_| f(r)).collect()
}

fn parse_defs(r: &mut Reader) -> Result<Vec<ZoneDef>> {
    let count = r.u32()?;
    array(r, count, |r| {
        let id = r.u32()?;
        let ini_path = utf16z(r)?;
        let (layer_count, model_count) = (r.u32()?, r.u32()?);
        let kind = array(r, layer_count, Reader::u32)?;
        let spacing = array(r, layer_count, Reader::f32)?;
        let collision_radius = array(r, layer_count, Reader::f32)?;
        let density = array(r, layer_count, Reader::f32)?;
        let scale_variance = array(r, layer_count, Reader::f32)?;
        let pattern = array(r, layer_count, Reader::u8)?;
        let models_in_layer = array(r, layer_count, Reader::u32)?;
        let layers = (0..layer_count as usize)
            .map(|i| ZoneLayer {
                kind: kind[i],
                spacing: spacing[i],
                collision_radius: collision_radius[i],
                density: density[i],
                scale_variance: scale_variance[i],
                pattern: pattern[i],
                model_count: models_in_layer[i],
            })
            .collect();
        let probability = array(r, model_count, Reader::f32)?;
        let flags = array(r, model_count, Reader::u32)?;
        let models = probability
            .into_iter()
            .zip(flags)
            .map(|(cumulative_probability, flags)| ZoneModel { cumulative_probability, flags })
            .collect();
        Ok(ZoneDef { id, ini_path, layers, models })
    })
}

fn parse_prop_files(data: &[u8]) -> Result<PropFiles> {
    let mut r = Reader::new(data);
    let n = r.u8()?;
    let models = array(&mut r, n as u32, Reader::u32)?;
    Ok(PropFiles { models, atlas: r.remaining().to_vec() })
}

fn parse_zones(r: &mut Reader) -> Result<Vec<Zone>> {
    let count = r.u32()?;
    array(r, count, |r| {
        let (def_id, flags, height_raw) = (r.u32()?, r.u8()?, r.u16()?);
        let n = r.u32()?;
        let vertices = array(r, n, |r| Ok([r.f32()?, r.f32()?]))?;
        Ok(Zone { def_id, flags, height_raw, vertices })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::testdata::MapPair;
    use crate::mapfile::{Ffna, parse_file_refs};

    #[test]
    fn ini_paths() {
        let path = "Chapter3\\Missions\\Nightmare\\Town\\Zones\\NightmareTownCreepy.ini";
        assert_eq!(ini_folder(path), "Chapter3\\Missions\\Nightmare\\Town");
        assert_eq!(ini_stem(path), "NightmareTownCreepy");
        assert_eq!(short_folder(path), "Nightmare\\Town");
        assert_eq!((ini_folder("Grass.ini"), ini_stem("Grass.ini"), short_folder("Grass.ini")), ("", "Grass", ""));
        assert_eq!(short_folder("Editor\\Zones\\Rocks.ini"), "Editor");
        assert_eq!(short_folder("Missions\\GuildWars\\CoastalTemple\\Zones\\TempleGrass.ini"), "GuildWars\\CoastalTemple");
    }

    #[test]
    fn polygon_geometry() {
        // A U shape, clockwise like the samples: the notch is outside.
        let zone = Zone {
            def_id: 1,
            flags: 0,
            height_raw: 0x8000,
            vertices: vec![[0.0, 0.0], [0.0, 3.0], [1.0, 3.0], [1.0, 1.0], [2.0, 1.0], [2.0, 3.0], [3.0, 3.0], [3.0, 0.0]],
        };
        assert_eq!(zone.height(), 0.0);
        assert_eq!(zone.signed_area(), -7.0);
        assert!(zone.contains([0.5, 2.5]) && zone.contains([1.5, 0.5]) && zone.contains([2.5, 2.0]));
        assert!(!zone.contains([1.5, 2.0]) && !zone.contains([-0.5, 1.0]) && !zone.contains([1.5, 3.5]));
        let [cx, cy] = zone.centroid();
        assert!((cx - 1.5).abs() < 1e-5 && (cy - 9.5 / 7.0).abs() < 1e-5, "{cx} {cy}");
    }

    #[test]
    fn layer_level() {
        let layer = |spacing| ZoneLayer {
            kind: 0,
            spacing,
            collision_radius: 0.0,
            density: 0.0,
            scale_variance: 0.0,
            pattern: 0,
            model_count: 0,
        };
        let levels: Vec<_> = [8.0, 9.6, 19.0, 30.0, 45.0, 80.0].map(|s| layer(s).level()).into();
        assert_eq!(levels, [0, 1, 1, 2, 3, 4]);
    }

    /// Every section parses without leftovers, and the defs consume the
    /// whole file-reference list.
    #[test]
    fn sample_maps() {
        for pair in MapPair::all() {
            let Some(strip) = pair.strip() else { continue };
            let file = Ffna::parse(&strip).unwrap();
            let zones = ZonesStrip::parse(file.chunk(0x1000_0003).unwrap()).unwrap();
            let refs = parse_file_refs(file.chunk(0x1100_0003).unwrap()).unwrap();
            assert_eq!(*zones.model_starts().last().unwrap(), refs.len(), "{pair:?}");
            assert!(!zones.zones.is_empty() && zones.unknown.is_empty(), "{pair:?}");
            for def in &zones.defs {
                let in_layers: u32 = def.layers.iter().map(|l| l.model_count).sum();
                assert_eq!(in_layers as usize, def.models.len(), "{pair:?} def {}", def.id);
                // Some layers end at 0.9999999.
                for (_, models) in def.layer_models() {
                    let last = models.last().unwrap().cumulative_probability;
                    assert!((last - 1.0).abs() < 1e-6, "{pair:?} def {}: {last}", def.id);
                }
            }
            for zone in &zones.zones {
                assert!(zones.defs.iter().any(|d| d.id == zone.def_id), "{pair:?}");
            }
        }
    }
}
