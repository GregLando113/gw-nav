//! Prop collision outlines: `PropCollision_Build` from the client's
//! `Engine\Map\Props\PrCollision.cpp`.
//!
//! Each prop's model file carries a collision outline (model chunk `0xBBA`,
//! which the client copies into the model's collision stream as `0xFA4`).
//! The outline is simplified, placed into the world by the prop's
//! rotation, scale and position, and split into ground and prop planes.

use crate::mapfile::props::{CollisionPoint, PropDef, PropsCollision, point_flag};
use crate::mapfile::{Ffna, MapFileError};

use super::X87;
use super::fastmath;

/// A model's collision outline point (16 bytes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPoint {
    /// Plane within the model; 0 = ground.
    pub plane: u8,
    /// `1` = plane start point, `2` = last point of a polygon.
    pub flags: u8,
    /// Portal id within the model; 0 = none.
    pub portal: u8,
    pub portal_plane: u8,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Model chunk with the collision outline (`u32 2, u32 n, n * 16 bytes`).
pub const MODEL_COLLISION_CHUNK: u32 = 0xBBA;
/// The same data in the client's generated collision stream.
pub const COLLISION_STREAM_CHUNK: u32 = 0xFA4;

/// Read the collision outline of a model file (FFNA type 2). Files that
/// are not models, and models without an outline, yield no points (as in
/// the client's `Model_parse_ffna_file`).
pub fn model_collision(model: &[u8]) -> Result<Vec<ModelPoint>, MapFileError> {
    let file = match Ffna::parse(model) {
        Ok(file) if file.file_type == 2 => file,
        Ok(_) | Err(MapFileError::NotFfna) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let Some(data) = file.chunk(MODEL_COLLISION_CHUNK).or_else(|| file.chunk(COLLISION_STREAM_CHUNK)) else {
        return Ok(Vec::new());
    };
    let mut r = crate::mapfile::tags::Reader::new(data);
    if data.len() < 8 || r.u32()? != 2 {
        return Ok(Vec::new());
    }
    let n = r.u32()?;
    (0..n)
        .map(|_| {
            Ok(ModelPoint {
                plane: r.u8()?,
                flags: r.u8()?,
                portal: r.u8()?,
                portal_plane: r.u8()?,
                x: r.f32()?,
                y: r.f32()?,
                z: r.f32()?,
            })
        })
        .collect()
}

/// `PropCollision_SimplifyVertices`: drop points within 2 units of the
/// line from the last kept point to the point after them, as long as
/// plane, flags and portal stay the same.
fn simplify(x87: X87, pts: &[ModelPoint]) -> Vec<ModelPoint> {
    let r = |v: f64| x87.r(v);
    let s = |v: f64| x87.f32(v) as f64;
    let mut out = Vec::with_capacity(pts.len());
    let mut i = 0;
    while i < pts.len() {
        let a = pts[i];
        out.push(a);
        i += 1;
        if a.flags & 3 != 0 {
            continue;
        }
        while i + 1 < pts.len() {
            let (n, m) = (pts[i], pts[i + 1]);
            if n.flags & 2 != 0 || n.flags != a.flags || n.plane != a.plane || n.portal != a.portal {
                break;
            }
            let (ax, ay) = (a.x as f64, a.y as f64);
            let dy = s(m.y as f64 - ay);
            let ex = s(ax - m.x as f64);
            let len2 = x87.f32(r(r(dy * dy) + r(ex * ex)));
            let rs = fastmath::rsqrt(len2) as f64;
            let nx = s(ex * rs);
            let ny = s(dy * rs);
            let d1 = s(r(r(n.x as f64 * ny) + r(n.y as f64 * nx)));
            let d2 = s(r(r(ay * nx) + r(ax * ny)));
            let dist = s(d1 - d2);
            if !(dist.abs() < 2.0) {
                break;
            }
            i += 1;
        }
    }
    out
}

/// The prop's yaw vector: the first two components of the first vector of
/// `Prop_calc_rotation_vectors`.
fn rotation(x87: X87, angles: [u8; 3]) -> [f64; 2] {
    let r = |v: f64| x87.r(v);
    let step = (std::f32::consts::TAU / 256.0) as f64;
    let a = angles.map(|b| x87.f32(r(b as f64 * step) + 0.0) as f64);
    let (s0, c0) = a[0].sin_cos();
    let (s1, c1) = a[1].sin_cos();
    let (s2, c2) = a[2].sin_cos();
    let c2s0 = r(c2 * s0);
    [x87.f32(r(r(s2 * c1) - r(c2s0 * s1))) as f64, x87.f32(r(c2 * c0)) as f64]
}

/// Build the stage-2 props collision data (tags 1-3) from the stage-1
/// props and their models' collision outlines.
pub fn build_collision<'a>(
    props: &[PropDef],
    model_points: impl Fn(u16) -> Option<&'a [ModelPoint]>,
    x87: X87,
) -> PropsCollision {
    let r = |v: f64| x87.r(v);
    let mut ground = Vec::new();
    let mut prop_planes = Vec::new();
    let mut plane_props: Vec<u16> = Vec::new();
    let mut plane_base = 0u32;
    let mut portal_base = 0u32;

    for (index, prop) in props.iter().enumerate() {
        let [px, py, pz] = prop.position;
        if !prop.points.is_empty() {
            for p in &prop.points {
                ground.push(CollisionPoint {
                    x: x87.f32(p[0] as f64 + px as f64),
                    y: x87.f32(p[1] as f64 + py as f64),
                    z: 0.0,
                    plane: 0,
                    flags: 0,
                    portal: 0,
                    portal_plane: 0,
                });
            }
            ground.last_mut().unwrap().flags = 2;
        }
        let Some(points) = model_points(prop.model).filter(|p| !p.is_empty()) else { continue };
        if prop.flags & 1 != 0 {
            continue;
        }

        let scale = x87.f32(r(prop.scale as f64 * (1.9921875 / 256.0)) + 0.0078125) as f64;
        let [rot0, rot1] = rotation(x87, prop.rotation);
        let mut prev = (f32::INFINITY, f32::INFINITY, f32::INFINITY);
        let mut prev_plane = 0xFFu8;
        let mut prev_ended = false;
        let (mut max_plane, mut max_portal) = (0u32, 0u32);
        for p in simplify(x87, points) {
            let (mx, my) = (p.x as f64, p.y as f64);
            let x = super::round(x87.f32(r(r(r(rot1 * mx) + r(rot0 * my)) * scale) + px as f64)) as f32;
            let y = super::round(x87.f32(r(r(r(rot1 * my) - r(rot0 * mx)) * scale) + py as f64)) as f32;
            let z = x87.f32(pz as f64 + r(p.z as f64 * scale));
            if prev == (x, y, z) && p.plane == prev_plane && !prev_ended {
                continue;
            }
            prev_plane = p.plane;
            prev_ended = p.flags & 2 != 0;
            let mut point = CollisionPoint {
                x,
                y,
                z,
                plane: if p.plane == 0 { 0 } else { p.plane as u32 + plane_base },
                flags: p.flags as u32,
                portal: 0,
                portal_plane: 0,
            };
            if p.portal != 0 {
                point.portal = (p.portal as u32 + portal_base) as u16;
                point.portal_plane = if p.portal_plane == 0 { 0 } else { (p.portal_plane as u32 + plane_base) as u16 };
                point.flags |= point_flag::PORTAL;
                max_portal = max_portal.max(p.portal as u32);
            }
            if p.plane == 0 { &mut ground } else { &mut prop_planes }.push(point);
            prev = (x, y, z);
            max_plane = max_plane.max(p.plane as u32);
        }
        portal_base += max_portal;
        if max_plane != 0 {
            let needed = (plane_base + 1 + max_plane) as usize;
            if plane_props.len() < needed {
                plane_props.resize(needed, 0);
            }
            for owner in &mut plane_props[plane_base as usize + 1..] {
                *owner = index as u16;
            }
            plane_base += max_plane;
        }
    }
    // PropCollision_Export: portal edges are a portal point and the next.
    let mut portal_points = Vec::new();
    for pair in ground.windows(2) {
        if pair[0].flags & point_flag::PORTAL != 0 {
            portal_points.push([pair[0].x, pair[0].y]);
            portal_points.push([pair[1].x, pair[1].y]);
        }
    }
    let mut points = ground;
    points.extend(prop_planes);
    PropsCollision { points, plane_props, portal_points }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::mapfile::parse_file_refs;
    use crate::mapfile::props::PropsStrip;
    use crate::mapfile::testdata::MapPair;

    /// Collision outlines of the models referenced by a map, from
    /// `testdata/models` (`cargo run -- fetch-models <id>`); `None` for
    /// models not downloaded.
    fn load_models(refs: &[u32]) -> Vec<Option<Vec<ModelPoint>>> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/models");
        let mut cache: HashMap<u32, Option<Vec<ModelPoint>>> = HashMap::new();
        refs.iter()
            .map(|id| {
                cache
                    .entry(*id)
                    .or_insert_with(|| {
                        let data = std::fs::read(dir.join(format!("{id}.ffna"))).ok()?;
                        Some(model_collision(&data).unwrap())
                    })
                    .clone()
            })
            .collect()
    }

    /// Our props collision must equal the client's (props tags 1-3). With
    /// models missing, only the part before the first prop that needs a
    /// missing model is checked.
    #[test]
    fn collision_matches_client() {
        for pair in MapPair::all() {
            let Some((strip, bloated)) = pair.chunks(0x1000_0004, 0x2000_0004) else { continue };
            let Some((refs, _)) = pair.chunks(0x1100_0004, 0x2100_0004) else { continue };
            let refs = parse_file_refs(&refs).unwrap();
            let models = load_models(&refs);
            let props = PropsStrip::parse(&strip).unwrap();
            let client = PropsCollision::parse_bloated(&bloated).unwrap();
            let missing = props.props.iter().position(|p| p.flags & 1 == 0 && models[p.model as usize].is_none());
            let complete = missing.is_none();
            let checked = &props.props[..missing.unwrap_or(props.props.len())];
            for x87 in [X87::Single, X87::Double] {
                let get = |m: u16| models[m as usize].as_deref();
                let ours = build_collision(checked, get, x87);
                let ground = |c: &PropsCollision| c.points.iter().filter(|p| p.plane == 0).cloned().collect::<Vec<_>>();
                let (og, cg) = (ground(&ours), ground(&client));
                let first_diff = og.iter().zip(&cg).position(|(a, b)| a != b);
                eprintln!(
                    "{pair:?} {x87:?}: {}/{} props checked, ground points {}/{} first diff {first_diff:?}, all points equal {}",
                    checked.len(),
                    props.props.len(),
                    og.len(),
                    cg.len(),
                    ours == client
                );
                if let Some(i) = first_diff {
                    eprintln!("   ours   {:?}
   client {:?}", &og[i..(i + 2).min(og.len())], &cg[i..(i + 2).min(cg.len())]);
                }
                if complete && x87 == X87::Double {
                    assert_eq!(ours, client, "{pair:?}");
                }
            }
        }
    }
}
