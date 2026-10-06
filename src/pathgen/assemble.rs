//! Path plane inputs: `PathChunk_BuildPathData`, `PathData_process_prop_segments`
//! and the grouping in `PathData_build_maps`.
//!
//! Terrain segments (from the tracer, already shuffled) come first, then
//! the prop outline segments in collision point order. Each plane gets the
//! contiguous run of segments, portals and start points with its index.

use crate::mapfile::props::{CollisionPoint, point_flag};

use super::tracer;

/// A 56-byte path segment. `p0` is the upper end (larger y, then larger x).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub p0: [f64; 2],
    pub p1: [f64; 2],
    pub vector: [f64; 2],
    pub plane: u16,
    /// Portal index within the plane, or `0xFFFF`.
    pub portal: u16,
}

impl Segment {
    pub fn is_portal(&self) -> bool {
        self.portal != 0xFFFF
    }
}

/// A 16-byte portal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalInput {
    pub plane: u16,
    pub neighbor_plane: u16,
    pub portal: u16,
    pub side_flags: u8,
}

/// A 24-byte start point record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartPoint {
    pub x: f64,
    pub y: f64,
    pub plane: u16,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaneInput {
    pub segments: Vec<Segment>,
    pub portals: Vec<PortalInput>,
    pub start_points: Vec<StartPoint>,
}

/// Snap a start point coordinate the way `PathChunk_BuildPathData` does:
/// round, divide by 4 towards zero, keep 16 bits, multiply by 4.
fn snap(v: f32) -> f64 {
    let i = super::round(v);
    let q = (i + ((i >> 31) & 3)) >> 2;
    ((q as i16 as i32) * 4) as f32 as f64
}

/// `PathData_add_segment`.
fn add_segment(a: &CollisionPoint, b: &CollisionPoint, portal_base: usize, out: &mut PlaneLists) {
    let a_lower = a.y < b.y || (a.y == b.y && a.x <= b.x);
    let (hi, lo) = if a_lower { (b, a) } else { (a, b) };
    let p0 = [hi.x as f64, hi.y as f64];
    let p1 = [lo.x as f64, lo.y as f64];
    let vector = [p1[0] - p0[0], p1[1] - p0[1]];
    if vector == [0.0, 0.0] {
        return; // "Degenerate pathing segment"
    }
    let mut seg = Segment { p0, p1, vector, plane: a.plane as u16, portal: 0xFFFF };
    if a.flags & point_flag::PORTAL != 0 {
        seg.portal = (out.portals.len() - portal_base) as u16;
        let mut side = 0u8;
        if a.flags & 0x8 != 0 {
            side = a_lower as u8 + 1;
        }
        if a.flags & 0x10 != 0 {
            side |= (!a_lower) as u8 + 1;
        }
        out.portals.push(PortalInput {
            plane: a.plane as u16,
            neighbor_plane: a.portal_plane,
            portal: a.portal,
            side_flags: side,
        });
    }
    out.segments.push(seg);
}

#[derive(Default)]
struct PlaneLists {
    segments: Vec<Segment>,
    portals: Vec<PortalInput>,
    start_points: Vec<StartPoint>,
}

/// Build every plane's input from the tracer's segments, the path chunk's
/// start points and the props collision points.
pub fn assemble(terrain: &[tracer::Segment], start_points: &[[f32; 2]], collision: &[CollisionPoint]) -> Vec<PlaneInput> {
    let mut lists = PlaneLists::default();
    lists.segments.extend(terrain.iter().map(|s| Segment {
        p0: s.p0,
        p1: s.p1,
        vector: s.vector,
        plane: 0,
        portal: 0xFFFF,
    }));
    lists.start_points.extend(start_points.iter().map(|p| StartPoint { x: snap(p[0]), y: snap(p[1]), plane: 0 }));

    // PathData_process_prop_segments.
    let mut current_plane = u32::MAX;
    let mut portal_base = 0;
    for (i, p) in collision.iter().enumerate() {
        if p.plane != current_plane {
            portal_base = lists.portals.len();
            current_plane = p.plane;
        }
        if p.flags & 1 != 0 {
            lists.start_points.push(StartPoint { x: p.x as f64, y: p.y as f64, plane: p.plane as u16 });
        } else if p.flags & 2 == 0 {
            if let Some(next) = collision.get(i + 1) {
                add_segment(p, next, portal_base, &mut lists);
            }
        }
    }

    // PathData_build_maps: contiguous runs per plane index.
    let mut planes = Vec::new();
    let (mut s, mut p, mut t) = (0, 0, 0);
    let mut index = 0u16;
    while s < lists.segments.len() {
        let mut plane = PlaneInput::default();
        while s < lists.segments.len() && lists.segments[s].plane == index {
            plane.segments.push(lists.segments[s]);
            s += 1;
        }
        while p < lists.portals.len() && lists.portals[p].plane == index {
            plane.portals.push(lists.portals[p]);
            p += 1;
        }
        while t < lists.start_points.len() && lists.start_points[t].plane == index {
            plane.start_points.push(lists.start_points[t]);
            t += 1;
        }
        assert!(
            s == lists.segments.len() || lists.segments[s].plane == index + 1,
            "mapIndex + 1 == segmentSeek->map"
        );
        planes.push(plane);
        index += 1;
    }
    planes
}
