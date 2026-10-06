//! Pathing planes: the contents of stage-2 path tag 8.
//!
//! Each plane is a trapezoidal map: a set of trapezoids plus a point
//! location DAG of X-nodes (segment tests), Y-nodes (horizontal splits) and
//! sink nodes (trapezoids). Plane 0 is the ground; the others come from
//! props such as bridges and are joined to other planes through portals.

use super::tags::{Reader, Writer};
use super::{MapFileError, Result};

pub mod tag {
    pub const COUNTS: u8 = 0;
    pub const VECTORS: u8 = 1;
    pub const TRAPEZOIDS: u8 = 2;
    pub const ROOT: u8 = 3;
    pub const X_NODES: u8 = 4;
    pub const Y_NODES: u8 = 5;
    pub const SINKS: u8 = 6;
    pub const PORTALS: u8 = 9;
    pub const PORTAL_TRAPEZOIDS: u8 = 10;
    pub const START_POINTS: u8 = 11;
}

/// Index value meaning "none" for u32 and u16 references.
pub const NONE_U32: u32 = u32::MAX;
pub const NONE_U16: u16 = u16::MAX;

/// A reference to a node: the top two bits give the node type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeRef {
    X(u32),
    Y(u32),
    Sink(u32),
    None,
}

impl NodeRef {
    pub fn from_raw(raw: u32) -> Self {
        if raw == NONE_U32 {
            return Self::None;
        }
        let index = raw & 0x3FFF_FFFF;
        match raw >> 30 {
            0 => Self::X(index),
            1 => Self::Y(index),
            2 => Self::Sink(index),
            _ => Self::None,
        }
    }

    pub fn to_raw(self) -> u32 {
        match self {
            Self::X(i) => i,
            Self::Y(i) => 1 << 30 | i,
            Self::Sink(i) => 2 << 30 | i,
            Self::None => NONE_U32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trapezoid {
    /// Neighbour trapezoid indices: top left, top right, bottom left,
    /// bottom right. [`NONE_U32`] if none.
    pub neighbors: [u32; 4],
    /// Portal indices of the left and right edges. [`NONE_U16`] if none.
    pub portal_left: u16,
    pub portal_right: u16,
    pub y_top: f32,
    pub y_bottom: f32,
    pub x_top_left: f32,
    pub x_top_right: f32,
    pub x_bottom_left: f32,
    pub x_bottom_right: f32,
}

/// Segment test: is the point left or right of the segment from
/// `vectors[v0]` to `vectors[v1]`?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XNode {
    pub v0: u32,
    pub v1: u32,
    pub left: NodeRef,
    pub right: NodeRef,
}

/// Horizontal split at `vectors[v].y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YNode {
    pub v: u32,
    pub above: NodeRef,
    pub below: NodeRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Portal {
    /// Range into [`PathPlane::portal_trapezoids`].
    pub count: u16,
    pub start: u16,
    pub neighbor_plane: u16,
    /// Portal id, shared with the matching portal of the neighbour plane:
    /// the one there whose `neighbor_plane` is this plane and whose `pair`
    /// is this same id.
    pub pair: u16,
    /// `0x4`: not used for pathfinding.
    pub flags: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathPlane {
    pub start_points: Vec<[f32; 2]>,
    pub vectors: Vec<[f32; 2]>,
    pub trapezoids: Vec<Trapezoid>,
    pub root: NodeRef,
    pub x_nodes: Vec<XNode>,
    pub y_nodes: Vec<YNode>,
    /// Trapezoid index per sink node.
    pub sinks: Vec<u32>,
    pub portal_trapezoids: Vec<u32>,
    pub portals: Vec<Portal>,
}

/// Read a stage-2 section and require its length to be `count * size`.
fn section<'a>(r: &mut Reader<'a>, tag: u8, count: u32, size: usize) -> Result<Reader<'a>> {
    let data = r.tag(tag)?;
    if data.len() != count as usize * size {
        return Err(MapFileError::Invalid("path plane section size"));
    }
    Ok(Reader::new(data))
}

fn vec2(r: &mut Reader) -> Result<[f32; 2]> {
    Ok([r.f32()?, r.f32()?])
}

impl PathPlane {
    fn read(r: &mut Reader) -> Result<Self> {
        let mut c = section(r, tag::COUNTS, 8, 4)?;
        let [start_count, vector_count, trap_count, x_count, y_count, sink_count, portal_count, portal_trap_count] =
            [c.u32()?, c.u32()?, c.u32()?, c.u32()?, c.u32()?, c.u32()?, c.u32()?, c.u32()?];

        // The length field of tag 11 is 16 * count, but only 8 * count
        // bytes follow (MapBrowser's parser does the same).
        r.strip_tag(tag::START_POINTS)?;
        let len = r.u32()?;
        if len != start_count * 16 {
            return Err(MapFileError::Invalid("path plane start point count"));
        }
        let start_points = (0..start_count).map(|_| vec2(r)).collect::<Result<_>>()?;

        let mut s = section(r, tag::VECTORS, vector_count, 8)?;
        let vectors = (0..vector_count).map(|_| vec2(&mut s)).collect::<Result<_>>()?;

        let mut s = section(r, tag::TRAPEZOIDS, trap_count, 44)?;
        let trapezoids = (0..trap_count)
            .map(|_| {
                Ok(Trapezoid {
                    neighbors: [s.u32()?, s.u32()?, s.u32()?, s.u32()?],
                    portal_left: s.u16()?,
                    portal_right: s.u16()?,
                    y_top: s.f32()?,
                    y_bottom: s.f32()?,
                    x_top_left: s.f32()?,
                    x_top_right: s.f32()?,
                    x_bottom_left: s.f32()?,
                    x_bottom_right: s.f32()?,
                })
            })
            .collect::<Result<_>>()?;

        // Tag 3 holds the root's node type; the root is node 0 of that type.
        let root = match section(r, tag::ROOT, 1, 1)?.u8()? {
            0 => NodeRef::X(0),
            1 => NodeRef::Y(0),
            2 => NodeRef::Sink(0),
            _ => return Err(MapFileError::Invalid("path plane root type")),
        };

        let mut s = section(r, tag::X_NODES, x_count, 16)?;
        let x_nodes = (0..x_count)
            .map(|_| {
                Ok(XNode {
                    v0: s.u32()?,
                    v1: s.u32()?,
                    left: NodeRef::from_raw(s.u32()?),
                    right: NodeRef::from_raw(s.u32()?),
                })
            })
            .collect::<Result<_>>()?;

        let mut s = section(r, tag::Y_NODES, y_count, 12)?;
        let y_nodes = (0..y_count)
            .map(|_| {
                Ok(YNode {
                    v: s.u32()?,
                    above: NodeRef::from_raw(s.u32()?),
                    below: NodeRef::from_raw(s.u32()?),
                })
            })
            .collect::<Result<_>>()?;

        let mut s = section(r, tag::SINKS, sink_count, 4)?;
        let sinks = (0..sink_count).map(|_| s.u32()).collect::<Result<_>>()?;

        let mut s = section(r, tag::PORTAL_TRAPEZOIDS, portal_trap_count, 4)?;
        let portal_trapezoids = (0..portal_trap_count).map(|_| s.u32()).collect::<Result<_>>()?;

        let mut s = section(r, tag::PORTALS, portal_count, 9)?;
        let portals = (0..portal_count)
            .map(|_| {
                Ok(Portal {
                    count: s.u16()?,
                    start: s.u16()?,
                    neighbor_plane: s.u16()?,
                    pair: s.u16()?,
                    flags: s.u8()?,
                })
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            start_points,
            vectors,
            trapezoids,
            root,
            x_nodes,
            y_nodes,
            sinks,
            portal_trapezoids,
            portals,
        })
    }
}

impl PathPlane {
    /// The client's per-plane writers (`PathChunk_WriteHeader` …
    /// `PathChunk_WritePortals`).
    fn write(&self, w: &mut Writer) {
        let mut counts = Writer::new();
        for n in [
            self.start_points.len(),
            self.vectors.len(),
            self.trapezoids.len(),
            self.x_nodes.len(),
            self.y_nodes.len(),
            self.sinks.len(),
            self.portals.len(),
            self.portal_trapezoids.len(),
        ] {
            counts.u32(n as u32);
        }
        w.tag(tag::COUNTS, counts.as_bytes());

        // The length field claims 16 bytes per point; 8 follow.
        w.u8(tag::START_POINTS).u32(self.start_points.len() as u32 * 16);
        for p in &self.start_points {
            w.f32(p[0]).f32(p[1]);
        }

        let open = w.begin_tag(tag::VECTORS);
        for v in &self.vectors {
            w.f32(v[0]).f32(v[1]);
        }
        w.end_tag(open);

        let open = w.begin_tag(tag::TRAPEZOIDS);
        for t in &self.trapezoids {
            for n in t.neighbors {
                w.u32(n);
            }
            w.u16(t.portal_left).u16(t.portal_right);
            for v in [t.y_top, t.y_bottom, t.x_top_left, t.x_top_right, t.x_bottom_left, t.x_bottom_right] {
                w.f32(v);
            }
        }
        w.end_tag(open);

        let root_type = match self.root {
            NodeRef::X(_) => 0,
            NodeRef::Y(_) => 1,
            NodeRef::Sink(_) | NodeRef::None => 2,
        };
        w.tag(tag::ROOT, &[root_type]);

        let open = w.begin_tag(tag::X_NODES);
        for x in &self.x_nodes {
            w.u32(x.v0).u32(x.v1).u32(x.left.to_raw()).u32(x.right.to_raw());
        }
        w.end_tag(open);

        let open = w.begin_tag(tag::Y_NODES);
        for y in &self.y_nodes {
            w.u32(y.v).u32(y.above.to_raw()).u32(y.below.to_raw());
        }
        w.end_tag(open);

        let open = w.begin_tag(tag::SINKS);
        for &t in &self.sinks {
            w.u32(t);
        }
        w.end_tag(open);

        let open = w.begin_tag(tag::PORTAL_TRAPEZOIDS);
        for &t in &self.portal_trapezoids {
            w.u32(t);
        }
        w.end_tag(open);

        let open = w.begin_tag(tag::PORTALS);
        for p in &self.portals {
            w.u16(p.count).u16(p.start).u16(p.neighbor_plane).u16(p.pair).u8(p.flags);
        }
        w.end_tag(open);
    }
}

/// Write the contents of stage-2 path tag 8 (`PathData_build_maps`).
pub fn write_planes(planes: &[PathPlane]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(planes.len() as u32);
    for plane in planes {
        plane.write(&mut w);
    }
    w.into_bytes()
}

/// Parse stage-2 path tag 8: `u32 plane_count`, then the planes.
pub fn parse_planes(data: &[u8]) -> Result<Vec<PathPlane>> {
    let mut r = Reader::new(data);
    let count = r.u32()?;
    let planes = (0..count).map(|_| PathPlane::read(&mut r)).collect::<Result<Vec<_>>>()?;
    if !r.is_empty() {
        return Err(MapFileError::Invalid("trailing data after path planes"));
    }
    Ok(planes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::path::PathBloated;
    use crate::mapfile::testdata::MapPair;

    #[test]
    fn node_refs_roundtrip() {
        for raw in [0, 5, 0x4000_0011, 0x8000_0000, NONE_U32] {
            assert_eq!(NodeRef::from_raw(raw).to_raw(), raw);
        }
        assert_eq!(NodeRef::from_raw(0x4000_0011), NodeRef::Y(0x11));
    }

    #[test]
    fn write_roundtrips_client_planes() {
        for pair in MapPair::all() {
            let Some((_, bloated)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let path = PathBloated::parse(&bloated).unwrap();
            let planes = parse_planes(path.planes).unwrap();
            assert!(write_planes(&planes) == path.planes, "{pair:?}");
        }
    }

    /// Structural checks on the client's planes: every index is in range
    /// and every trapezoid is well formed.
    #[test]
    fn parses_bloated_planes() {
        for pair in MapPair::all() {
            let Some((_, bloated)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let path = PathBloated::parse(&bloated).unwrap();
            let planes = parse_planes(path.planes).unwrap();
            assert_eq!(planes.len(), path.plane_indices.len(), "{pair:?}");

            for (i, plane) in planes.iter().enumerate() {
                let node_ok = |n: NodeRef| match n {
                    NodeRef::X(i) => (i as usize) < plane.x_nodes.len(),
                    NodeRef::Y(i) => (i as usize) < plane.y_nodes.len(),
                    NodeRef::Sink(i) => (i as usize) < plane.sinks.len(),
                    NodeRef::None => true,
                };
                let vector_ok = |v: u32| (v as usize) < plane.vectors.len();
                assert!(node_ok(plane.root));
                for x in &plane.x_nodes {
                    assert!(vector_ok(x.v0) && vector_ok(x.v1) && node_ok(x.left) && node_ok(x.right));
                }
                for y in &plane.y_nodes {
                    assert!(vector_ok(y.v) && node_ok(y.above) && node_ok(y.below));
                }
                let traps = plane.trapezoids.len() as u32;
                assert!(plane.sinks.iter().all(|&t| t < traps));
                assert!(plane.portal_trapezoids.iter().all(|&t| t < traps));
                for t in &plane.trapezoids {
                    assert!(t.neighbors.iter().all(|&n| n == NONE_U32 || n < traps));
                    assert!(t.y_top >= t.y_bottom, "{pair:?} plane {i}: {t:?}");
                    assert!(t.x_top_left <= t.x_top_right && t.x_bottom_left <= t.x_bottom_right);
                }
                for p in &plane.portals {
                    assert!(p.start as usize + p.count as usize <= plane.portal_trapezoids.len());
                    assert!((p.neighbor_plane as usize) < planes.len());
                }
            }
            let traps: usize = planes.iter().map(|p| p.trapezoids.len()).sum();
            eprintln!("{pair:?}: {} planes, {traps} trapezoids", planes.len());
        }
    }
}
