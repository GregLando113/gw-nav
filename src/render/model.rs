//! Prop models as the fileserver serves them: FFNA type 2 files with the
//! `0xBB8` geometry chunk (which the client converts to `0xFA0` in
//! `MdlDecomp_ConvertGeometryChunk_0xBB8_to_0xFA0 @799b80`) and the `0xBBB`
//! texture list.
//!
//! Only what a top-down render needs is decoded: each submesh's triangles,
//! positions, UV sets and base texture. Layout from the client
//! (`MdlDecomp_ConvertSubmesh @79a3e0`) and GuildWarsMapBrowser
//! (`FFNA_ModelFile_Other.h`, `FFNA_ModelFile.h`).

use crate::mapfile::tags::Reader;
use crate::mapfile::{Ffna, MapFileError};

pub const GEOMETRY_CHUNK: u32 = 0xBB8;
pub const TEXTURES_CHUNK: u32 = 0xBBB;

mod class {
    pub const BONE_GROUPS: u32 = 0x002;
    pub const SUBMESHES: u32 = 0x008;
    pub const EMBEDDED_ANIMATION: u32 = 0x020;
    pub const BONE_WEIGHTS: u32 = 0x040;
    pub const MORPH_TARGETS: u32 = 0x080;
}

/// A submesh's base texture: an index into [`Model::textures`] and the UV
/// set it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureSlot {
    pub texture: u8,
    pub uv_set: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Submesh {
    /// Triangle list (the full-detail LOD).
    pub indices: Vec<u16>,
    /// Model space: x, y, and z pointing down.
    pub positions: Vec<[f32; 3]>,
    /// Per UV set, one coordinate per vertex.
    pub uvs: Vec<Vec<[f32; 2]>>,
    pub texture: Option<TextureSlot>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    pub submeshes: Vec<Submesh>,
    /// Base file ids of the model's textures (`0xBBB`).
    pub textures: Vec<u32>,
}

/// Parse a model file. Files that are not models, or whose geometry is in
/// a layout not handled here (bone weights, morph targets), give `None`.
pub fn parse(file: &[u8]) -> Option<Model> {
    let ffna = Ffna::parse(file).ok().filter(|f| f.file_type == 2)?;
    let textures = ffna.chunk(TEXTURES_CHUNK).and_then(|c| texture_refs(c).ok()).unwrap_or_default();
    let submeshes = geometry(ffna.chunk(GEOMETRY_CHUNK)?, textures.len()).ok()??;
    Some(Model { submeshes, textures })
}

/// Base file ids in a `0xBBB` chunk: `u32`, `u32 count`, then 6-byte file
/// references as in the map's file-reference lists.
pub fn texture_refs(chunk: &[u8]) -> Result<Vec<u32>, MapFileError> {
    let mut r = Reader::new(chunk);
    r.u32()?;
    let n = r.u32()? as usize;
    (0..n.min(chunk.len() / 6))
        .map(|_| {
            let (w0, w1) = (r.u16()? as u32, r.u16()? as u32);
            r.u16()?;
            Ok(w0.wrapping_sub(0xFF_00FF).wrapping_add(w1.wrapping_mul(0xFF00)))
        })
        .collect()
}

/// How submeshes pick their base texture.
enum Materials {
    /// Prophecies/Factions: per shader, a run of texture slots.
    Old { slots_per_shader: Vec<u8>, uv_sets: Vec<u8>, textures: Vec<u8> },
    /// Nightfall/EotN: per texture group, a run of texture slots.
    Modern { slots_per_group: Vec<u8>, textures: Vec<u8> },
    None,
}

impl Materials {
    fn slot(&self, material: u32, texture_count: usize) -> Option<TextureSlot> {
        match self {
            Materials::Old { slots_per_shader, uv_sets, textures } => {
                let shader = material as usize % slots_per_shader.len().max(1);
                let start: usize = slots_per_shader.iter().take(shader).map(|&n| n as usize).sum();
                let count = *slots_per_shader.get(shader)? as usize;
                // The first slot that maps a texture: 255 skips the slot, 253
                // takes the UV set of the next one.
                (start..start + count).find_map(|i| {
                    let uv = match *uv_sets.get(i)? {
                        255 => return None,
                        253 => *uv_sets.get(i + 1).filter(|&&u| u < 8)?,
                        u => u,
                    };
                    Some(TextureSlot { texture: *textures.get(i)?, uv_set: uv })
                })
            }
            Materials::Modern { slots_per_group, textures } => {
                let group = material as usize % slots_per_group.len().max(1);
                let start: usize = slots_per_group.iter().take(group).map(|&n| n as usize).sum();
                let mut t = *textures.get(start)?;
                if t as usize >= texture_count {
                    t &= 0x0F;
                }
                Some(TextureSlot { texture: t, uv_set: 0 })
            }
            Materials::None => (texture_count > 0).then_some(TextureSlot { texture: 0, uv_set: 0 }),
        }
    }
}

/// The submeshes of a `0xBB8` chunk, or `None` for layouts not handled.
fn geometry(chunk: &[u8], texture_count: usize) -> Result<Option<Vec<Submesh>>, MapFileError> {
    let mut h = Reader::new(chunk);
    h.bytes(8)?;
    let class = h.u32()?;
    h.bytes(12)?;
    let shader_count = h.u8()? as usize;
    let texture_groups = h.u8()? as usize;
    let texture_names = h.u16()? as usize;
    let slot_count = h.u8()? as usize;
    let group_slots = h.u8()? as usize;
    let material_names = h.u16()? as usize;
    let weight_sets = h.u32()?;
    if class & (class::BONE_WEIGHTS | class::MORPH_TARGETS) != 0 || class & class::SUBMESHES == 0 {
        return Ok(None);
    }
    let mut r = Reader::new(&chunk[0x30..]);
    if class & class::BONE_GROUPS != 0 {
        let n = r.u32()? as usize;
        r.bytes(n * 28)?;
    }
    let shaders = r.bytes(shader_count * 8)?;
    // Slot arrays: u16 flags, u8 UV set, 4 zero bytes, u8 blend, u8 texture,
    // and one more byte per slot with weight sets.
    let slots = r.bytes(slot_count * 9 + if weight_sets != 0 { slot_count } else { 0 })?;
    let mut materials = if shader_count > 0 && slot_count > 0 {
        Materials::Old {
            slots_per_shader: shaders.chunks_exact(8).map(|s| s[7]).collect(),
            uv_sets: slots[2 * slot_count..3 * slot_count].to_vec(),
            textures: slots[8 * slot_count..9 * slot_count].to_vec(),
        }
    } else {
        Materials::None
    };
    if texture_groups != 0xFF && texture_names < 0x100 && material_names < 0x100 && texture_groups > 0 {
        let groups = r.bytes(texture_groups * 9)?;
        // Per slot: u16 flags, u8 texture, and with weight sets one more byte.
        let table = r.bytes(group_slots * if weight_sets != 0 { 4 } else { 3 })?;
        materials = Materials::Modern {
            slots_per_group: groups.chunks_exact(9).map(|g| g[6]).collect(),
            textures: table[2 * group_slots..3 * group_slots].to_vec(),
        };
        r.bytes(texture_names * 8)?;
        for _ in 0..texture_names {
            while r.u8()? != 0 {}
        }
        r.bytes(material_names * 8)?;
    }
    if class & class::EMBEDDED_ANIMATION != 0 {
        let size = r.u32()? as usize;
        r.u32()?;
        r.bytes(size)?;
    }

    let count = r.u32()?;
    if count > 0xFE {
        return Err(MapFileError::Invalid("submesh count"));
    }
    let mut submeshes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let [flags, material, index_count, vertex_count, uv_sets, groups, group_bones, triangle_groups] =
            std::array::from_fn(|_| r.u32().unwrap_or(0));
        if vertex_count == 0 || index_count == 0 || uv_sets > 8 {
            return Err(MapFileError::Invalid("submesh header"));
        }
        let (nv, ni) = (vertex_count as usize, index_count as usize);
        let indices: Vec<u16> = r.bytes(ni * 2)?.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        let positions: Vec<[f32; 3]> = r
            .bytes(nv * 12)?
            .chunks_exact(12)
            .map(|b| std::array::from_fn(|i| f32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())))
            .collect();
        r.bytes(nv * 4)?;
        // Two optional 4-byte vertex streams (normals and colours).
        for bit in [0x8, 0x10] {
            if flags & bit != 0 {
                r.bytes(nv * 4)?;
            }
        }
        let uvs = read_uvs(&mut r, nv, uv_sets as usize)?;
        r.bytes(groups as usize * 3 + group_bones as usize + triangle_groups as usize * 12)?;
        if indices.iter().any(|&i| i as usize >= nv) {
            return Err(MapFileError::Invalid("submesh index"));
        }
        submeshes.push(Submesh { indices, positions, uvs, texture: materials.slot(material, texture_count) });
    }
    Ok(Some(submeshes))
}

/// UV sets: `u16 runs_u, u16 runs_v`, run lengths and integer offsets for
/// each, then `u16` fractions per vertex and set. A coordinate is
/// `fraction / 65535 + offset`, the offset stepping to the next run after
/// each run's length; runs continue across the sets.
fn read_uvs(r: &mut Reader, vertices: usize, sets: usize) -> Result<Vec<Vec<[f32; 2]>>, MapFileError> {
    let (cu, cv) = (r.u16()? as usize, r.u16()? as usize);
    let table = r.bytes((cu + cv) * 4)?;
    let u16_at = |i: usize| u16::from_le_bytes([table[2 * i], table[2 * i + 1]]);
    let (u_len, v_len) = ((0..cu).map(u16_at).collect::<Vec<_>>(), (cu..cu + cv).map(u16_at).collect::<Vec<_>>());
    let (u_off, v_off) = (
        (cu + cv..2 * cu + cv).map(|i| u16_at(i) as i16).collect::<Vec<_>>(),
        (2 * cu + cv..2 * (cu + cv)).map(|i| u16_at(i) as i16).collect::<Vec<_>>(),
    );
    let data = r.bytes(vertices * sets * 4)?;
    let (mut ui, mut vi, mut un, mut vn) = (0usize, 0usize, 0u32, 0u32);
    let mut out = vec![Vec::with_capacity(vertices); sets];
    for (k, b) in data.chunks_exact(4).enumerate() {
        let (fu, fv) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
        let u = fu as f32 * 1.525_902_2e-5 + *u_off.get(ui).unwrap_or(&0) as f32;
        let v = fv as f32 * 1.525_902_2e-5 + *v_off.get(vi).unwrap_or(&0) as f32;
        out[k / vertices].push([u, v]);
        un += 1;
        vn += 1;
        if u_len.get(ui).is_some_and(|&l| l as u32 == un) {
            ui += 1;
            un = 0;
        }
        if v_len.get(vi).is_some_and(|&l| l as u32 == vn) {
            vi += 1;
            vn = 0;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference models (`testdata/models`, from `fetch-models`) parse,
    /// and their texture slots point into their texture lists.
    #[test]
    fn parses_reference_models() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join("models");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let (mut models, mut parsed, mut triangles) = (0, 0, 0);
        for entry in entries.flatten() {
            let data = std::fs::read(entry.path()).unwrap();
            if Ffna::parse(&data).map_or(true, |f| f.file_type != 2) {
                continue;
            }
            models += 1;
            let Some(model) = parse(&data) else { continue };
            parsed += 1;
            for s in &model.submeshes {
                triangles += s.indices.len() / 3;
                assert_eq!(s.uvs.iter().map(Vec::len).sum::<usize>(), s.uvs.len() * s.positions.len());
                if let Some(t) = s.texture {
                    assert!((t.texture as usize) < model.textures.len().max(1), "{:?}: {t:?}", entry.path());
                }
            }
        }
        eprintln!("{parsed}/{models} models parsed, {triangles} triangles");
        assert!(parsed * 10 >= models * 9, "{parsed}/{models}");
    }
}
