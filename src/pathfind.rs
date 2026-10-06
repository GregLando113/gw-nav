//! Shortest paths over the pathing planes.
//!
//! The walkable area is the union of the trapezoids of all planes. A
//! trapezoid connects to its neighbours above and below in the same plane
//! ([`Trapezoid::neighbors`]) where their edges overlap, and to the
//! trapezoids of another plane through portals: a side on portal `p` meets
//! the trapezoids listed by the matching portal of the neighbour plane (the
//! one there with `neighbor_plane` pointing back and the same id in `pair`)
//! where the sides overlap. These shared stretches of edge are the *gates*;
//! the rest of each trapezoid's boundary is wall.
//!
//! [`NavMesh::route`] finds the exact Euclidean shortest path. A shortest
//! path only bends at *corners*, the endpoints of wall pieces, so it is an
//! A* search over the corners visible from each other (a lazily built
//! visibility graph). The corners visible from a point are found by casting
//! a cone through each gate of the trapezoids holding the point and
//! narrowing it at every further gate it passes, like a funnel.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;

use crate::mapfile::navmesh::{NONE_U32, PathPlane, Trapezoid};
use crate::waypoint::Waypoint;

/// Distance (world units) within which points count as touching. The map
/// data is f32 and the two sides of a portal differ by up to about 0.01.
const TOL: f64 = 0.02;

/// Upper bound on cone steps for one visibility query, against runaway
/// searches on malformed data.
const MAX_CONE_STEPS: usize = 4_000_000;

/// Narrowest opening (world units) a cone passes through.
const MIN_GAP: f64 = 1e-3;

type P = [f64; 2];

fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}

fn cross(a: P, b: P) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

fn dist(a: P, b: P) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn lerp(a: P, b: P, t: f64) -> P {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Signed distance of `v` from the line through `r` towards `d`, positive
/// on the left.
fn side(r: P, d: P, v: P) -> f64 {
    let dir = sub(d, r);
    let len = dir[0].hypot(dir[1]);
    if len == 0.0 { 0.0 } else { cross(dir, sub(v, r)) / len }
}

/// Distance from `v` to the segment `a`–`b`.
fn segment_distance(v: P, a: P, b: P) -> f64 {
    let ab = sub(b, a);
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 == 0.0 { 0.0 } else { ((v[0] - a[0]) * ab[0] + (v[1] - a[1]) * ab[1]) / len2 };
    dist(v, lerp(a, b, t.clamp(0.0, 1.0)))
}

#[derive(Debug, Clone, Copy)]
struct Trap {
    plane: u32,
    y_top: f64,
    y_bottom: f64,
    x_top_left: f64,
    x_top_right: f64,
    x_bottom_left: f64,
    x_bottom_right: f64,
}

impl Trap {
    fn new(plane: u32, t: &Trapezoid) -> Self {
        Self {
            plane,
            y_top: t.y_top.into(),
            y_bottom: t.y_bottom.into(),
            x_top_left: t.x_top_left.into(),
            x_top_right: t.x_top_right.into(),
            x_bottom_left: t.x_bottom_left.into(),
            x_bottom_right: t.x_bottom_right.into(),
        }
    }

    /// The x of the left or right side at `y`.
    fn side_x(&self, left: bool, y: f64) -> f64 {
        let h = self.y_top - self.y_bottom;
        let f = if h > 0.0 { (y - self.y_bottom) / h } else { 0.0 };
        if left {
            self.x_bottom_left + (self.x_top_left - self.x_bottom_left) * f
        } else {
            self.x_bottom_right + (self.x_top_right - self.x_bottom_right) * f
        }
    }

    /// `true` if `p` is inside, or within `tol` of it.
    fn contains(&self, [x, y]: P, tol: f64) -> bool {
        if y < self.y_bottom - tol || y > self.y_top + tol {
            return false;
        }
        let y = y.clamp(self.y_bottom, self.y_top);
        x >= self.side_x(true, y) - tol && x <= self.side_x(false, y) + tol
    }
}

/// A side of a trapezoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

impl Edge {
    /// The side of the trapezoid across a gate on this side. Portals always
    /// join a left side to a right side.
    fn across(self) -> Self {
        match self {
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
        }
    }
}

/// A stretch of boundary shared with trapezoid `to`.
#[derive(Debug, Clone, Copy)]
struct Gate {
    a: P,
    b: P,
    to: u32,
    edge: Edge,
}

#[derive(Debug, Clone, Copy)]
struct Corner {
    p: P,
    plane: u32,
    /// A trapezoid it lies on.
    trap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("the start is not on a plane")]
    StartOffMesh,
    #[error("the destination is not on a plane")]
    GoalOffMesh,
    #[error("the destination can't be reached from the start")]
    Unreachable,
}

/// A shortest path: the start, the corners it bends at, and the
/// destination.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub points: Vec<Waypoint>,
    pub length: f32,
}

/// The walkable area of a map, prepared for path queries.
pub struct NavMesh {
    traps: Vec<Trap>,
    /// Global index of each plane's first trapezoid.
    plane_start: Vec<u32>,
    gate_ranges: Vec<Range<u32>>,
    gates: Vec<Gate>,
    corner_ranges: Vec<Range<u32>>,
    trap_corners: Vec<u32>,
    corners: Vec<Corner>,
}

/// The parts of `lo..=hi` not covered by `covered`, as `(start, end)`.
/// A degenerate range that isn't covered is returned as a single point.
fn uncovered(lo: f64, hi: f64, covered: &mut [(f64, f64)]) -> Vec<(f64, f64)> {
    covered.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    let mut at = lo;
    for &(c0, c1) in covered.iter() {
        if c0 > at {
            out.push((at, c0.min(hi)));
        }
        at = at.max(c1);
    }
    if at < hi || (out.is_empty() && covered.is_empty()) {
        out.push((at, hi));
    }
    out
}

impl NavMesh {
    pub fn new(planes: &[PathPlane]) -> Self {
        Self::build(planes, true)
    }

    /// `prune`: leave out the corners no shortest path bends at.
    fn build(planes: &[PathPlane], prune: bool) -> Self {
        let mut plane_start = Vec::with_capacity(planes.len());
        let mut traps = Vec::new();
        for (i, plane) in planes.iter().enumerate() {
            plane_start.push(traps.len() as u32);
            traps.extend(plane.trapezoids.iter().map(|t| Trap::new(i as u32, t)));
        }
        let global = |plane: usize, t: u32| plane_start[plane] + t;
        let mut trap_gates: Vec<Vec<Gate>> = vec![Vec::new(); traps.len()];

        // Neighbours above and below.
        for (pi, plane) in planes.iter().enumerate() {
            for (ti, t) in plane.trapezoids.iter().enumerate() {
                let here = &traps[global(pi, ti as u32) as usize];
                for (k, &n) in t.neighbors.iter().enumerate() {
                    if n == NONE_U32 || n as usize >= plane.trapezoids.len() {
                        continue;
                    }
                    let other = &traps[global(pi, n) as usize];
                    let (edge, y, lo, hi) = if k < 2 {
                        let (lo, hi) = (here.x_top_left.max(other.x_bottom_left), here.x_top_right.min(other.x_bottom_right));
                        (Edge::Top, here.y_top, lo, hi)
                    } else {
                        let (lo, hi) = (here.x_bottom_left.max(other.x_top_left), here.x_bottom_right.min(other.x_top_right));
                        (Edge::Bottom, here.y_bottom, lo, hi)
                    };
                    if hi > lo {
                        let gate = Gate { a: [lo, y], b: [hi, y], to: global(pi, n), edge };
                        trap_gates[global(pi, ti as u32) as usize].push(gate);
                    }
                }
            }
        }

        // Portals between planes (or within one).
        for (pi, plane) in planes.iter().enumerate() {
            for (p_index, portal) in plane.portals.iter().enumerate() {
                let b = portal.neighbor_plane as usize;
                let Some(other) = planes.get(b) else { continue };
                let Some((q_index, pair)) = other
                    .portals
                    .iter()
                    .enumerate()
                    .find(|(_, q)| q.neighbor_plane as usize == pi && q.pair == portal.pair)
                else {
                    continue;
                };
                if (portal.flags | pair.flags) & 4 != 0 {
                    continue;
                }
                let listed = |plane: &PathPlane, start: u16, count: u16| {
                    let range = start as usize..(start as usize + count as usize).min(plane.portal_trapezoids.len());
                    plane.portal_trapezoids.get(range).unwrap_or(&[]).to_vec()
                };
                for ti in listed(plane, portal.start, portal.count) {
                    let Some(t) = plane.trapezoids.get(ti as usize) else { continue };
                    let left = if t.portal_left == p_index as u16 {
                        true
                    } else if t.portal_right == p_index as u16 {
                        false
                    } else {
                        continue;
                    };
                    let here = traps[global(pi, ti) as usize];
                    for ui in listed(other, pair.start, pair.count) {
                        let Some(u) = other.trapezoids.get(ui as usize) else { continue };
                        if u.portal_left != q_index as u16 && u.portal_right != q_index as u16 {
                            continue;
                        }
                        let there = &traps[global(b, ui) as usize];
                        let (y0, y1) = (here.y_bottom.max(there.y_bottom), here.y_top.min(there.y_top));
                        if y1 > y0 {
                            trap_gates[global(pi, ti) as usize].push(Gate {
                                a: [here.side_x(left, y0), y0],
                                b: [here.side_x(left, y1), y1],
                                to: global(b, ui),
                                edge: if left { Edge::Left } else { Edge::Right },
                            });
                        }
                    }
                }
            }
        }

        // The angle of walkable area around each boundary point of a plane,
        // summed over the trapezoids meeting there. A shortest path only
        // bends where it exceeds half a turn.
        let key = |plane: u32, p: P| (plane, p[0].to_bits(), p[1].to_bits());
        let mut open_angle: HashMap<(u32, u64, u64), f64> = HashMap::new();
        for (i, t) in traps.iter().enumerate() {
            let mut add = |p: P, angle: f64| *open_angle.entry(key(t.plane, p)).or_default() += angle;
            let mut vertices = vec![
                [t.x_bottom_left, t.y_bottom],
                [t.x_bottom_right, t.y_bottom],
                [t.x_top_right, t.y_top],
                [t.x_top_left, t.y_top],
            ];
            vertices.dedup();
            if vertices.len() > 1 && vertices[0] == vertices[vertices.len() - 1] {
                vertices.pop();
            }
            let flat = t.y_top <= t.y_bottom || vertices.len() < 3;
            for (k, &v) in vertices.iter().enumerate() {
                let (prev, next) = (vertices[(k + vertices.len() - 1) % vertices.len()], vertices[(k + 1) % vertices.len()]);
                let (a, b) = (sub(prev, v), sub(next, v));
                // Flat trapezoids count as half a turn: never too little.
                let angle = if flat { std::f64::consts::PI } else { cross(a, b).abs().atan2(a[0] * b[0] + a[1] * b[1]) };
                add(v, angle);
            }
            for g in &trap_gates[i] {
                for p in [g.a, g.b] {
                    if !vertices.contains(&p) {
                        add(p, std::f64::consts::PI);
                    }
                }
            }
        }

        // Portal gates by plane and grid cell. By a portal the walkable area
        // continues on the other plane, whose share of the angle isn't
        // counted: corners there are always kept.
        const CELL: f64 = 64.0;
        let cell = |v: f64| (v / CELL).floor() as i32;
        let mut portal_cells: HashMap<(u32, i32, i32), Vec<(P, P)>> = HashMap::new();
        for (i, t) in traps.iter().enumerate() {
            for g in trap_gates[i].iter().filter(|g| matches!(g.edge, Edge::Left | Edge::Right)) {
                let (x0, x1) = (cell(g.a[0].min(g.b[0]) - TOL), cell(g.a[0].max(g.b[0]) + TOL));
                let (y0, y1) = (cell(g.a[1].min(g.b[1]) - TOL), cell(g.a[1].max(g.b[1]) + TOL));
                for cx in x0..=x1 {
                    for cy in y0..=y1 {
                        portal_cells.entry((t.plane, cx, cy)).or_default().push((g.a, g.b));
                    }
                }
            }
        }
        let by_portal = |plane: u32, p: P| {
            portal_cells
                .get(&(plane, cell(p[0]), cell(p[1])))
                .is_some_and(|gates| gates.iter().any(|&(a, b)| segment_distance(p, a, b) <= TOL))
        };

        // Corners: the endpoints of the wall pieces of each trapezoid.
        let mut corners = Vec::new();
        let mut corner_ids: HashMap<(u32, u64, u64), u32> = HashMap::new();
        let mut trap_corners = Vec::new();
        let mut corner_ranges = Vec::with_capacity(traps.len());
        for (i, t) in traps.iter().enumerate() {
            let gates = &trap_gates[i];
            let mut points: Vec<P> = Vec::new();
            if t.y_top <= t.y_bottom {
                // Flat: keep all four vertices.
                points.extend([
                    [t.x_top_left, t.y_top],
                    [t.x_top_right, t.y_top],
                    [t.x_bottom_left, t.y_bottom],
                    [t.x_bottom_right, t.y_bottom],
                ]);
            } else {
                for (y, lo, hi) in [(t.y_top, t.x_top_left, t.x_top_right), (t.y_bottom, t.x_bottom_left, t.x_bottom_right)]
                {
                    let edge = if y == t.y_top { Edge::Top } else { Edge::Bottom };
                    let mut covered: Vec<(f64, f64)> = gates
                        .iter()
                        .filter(|g| g.edge == edge)
                        .map(|g| (g.a[0].min(g.b[0]), g.a[0].max(g.b[0])))
                        .collect();
                    for (x0, x1) in uncovered(lo, hi, &mut covered) {
                        points.extend([[x0, y], [x1, y]]);
                    }
                }
                for left in [true, false] {
                    let edge = if left { Edge::Left } else { Edge::Right };
                    let mut covered: Vec<(f64, f64)> = gates
                        .iter()
                        .filter(|g| g.edge == edge)
                        .map(|g| (g.a[1].min(g.b[1]), g.a[1].max(g.b[1])))
                        .collect();
                    for (y0, y1) in uncovered(t.y_bottom, t.y_top, &mut covered) {
                        points.extend([[t.side_x(left, y0), y0], [t.side_x(left, y1), y1]]);
                    }
                }
            }
            let start = trap_corners.len() as u32;
            for p in points {
                if prune && open_angle[&key(t.plane, p)] <= std::f64::consts::PI + 1e-6 && !by_portal(t.plane, p) {
                    continue;
                }
                let id = *corner_ids.entry(key(t.plane, p)).or_insert_with(|| {
                    corners.push(Corner { p, plane: t.plane, trap: i as u32 });
                    corners.len() as u32 - 1
                });
                if !trap_corners[start as usize..].contains(&id) {
                    trap_corners.push(id);
                }
            }
            corner_ranges.push(start..trap_corners.len() as u32);
        }

        let mut gates = Vec::new();
        let mut gate_ranges = Vec::with_capacity(traps.len());
        for list in trap_gates {
            let start = gates.len() as u32;
            gates.extend(list);
            gate_ranges.push(start..gates.len() as u32);
        }
        Self { traps, plane_start, gate_ranges, gates, corner_ranges, trap_corners, corners }
    }

    fn gates(&self, trap: u32) -> &[Gate] {
        let r = &self.gate_ranges[trap as usize];
        &self.gates[r.start as usize..r.end as usize]
    }

    fn trap_corners(&self, trap: u32) -> &[u32] {
        let r = &self.corner_ranges[trap as usize];
        &self.trap_corners[r.start as usize..r.end as usize]
    }

    pub fn plane_count(&self) -> usize {
        self.plane_start.len()
    }

    /// Number of corners (possible bends of a shortest path).
    pub fn corner_count(&self) -> usize {
        self.corners.len()
    }

    /// The trapezoid (global index) holding `p` on `plane`, else on the
    /// highest-numbered plane holding it.
    fn locate(&self, p: P, plane: u32) -> Option<u32> {
        let in_plane = |plane: usize| -> Option<u32> {
            let start = *self.plane_start.get(plane)?;
            let end = self.plane_start.get(plane + 1).copied().unwrap_or(self.traps.len() as u32);
            (start..end).find(|&i| self.traps[i as usize].contains(p, 0.0))
                .or_else(|| (start..end).find(|&i| self.traps[i as usize].contains(p, TOL)))
        };
        in_plane(plane as usize).or_else(|| (0..self.plane_count()).rev().filter(|&i| i != plane as usize).find_map(in_plane))
    }

    /// The plane `p` is on: `plane` if it holds `p`, else the highest-numbered
    /// plane holding it.
    pub fn plane_at(&self, [x, y]: [f32; 2], plane: u32) -> Option<u32> {
        self.locate([x.into(), y.into()], plane).map(|t| self.traps[t as usize].plane)
    }

    /// All trapezoids holding `p`, starting from `seed`: those reached
    /// through gates that `p` lies on.
    fn incident(&self, p: P, seed: u32) -> Vec<u32> {
        let mut found = vec![seed];
        let mut i = 0;
        while i < found.len() {
            for g in self.gates(found[i]) {
                if !found.contains(&g.to) && segment_distance(p, g.a, g.b) <= TOL {
                    found.push(g.to);
                }
            }
            i += 1;
        }
        found
    }

    /// Call `seen` with each corner visible from `r` (which lies on the
    /// trapezoids `incident`), and with `None` if the goal is: `goal` on
    /// the trapezoids `goal_traps`.
    fn visible(&self, r: P, incident: &[u32], goal: P, goal_traps: &[u32], mut seen: impl FnMut(Option<u32>)) {
        /// The rays from `r` between `right` and `left` (less than half a
        /// turn apart), entering `trap` from `from` on its side `entry`.
        struct Cone {
            trap: u32,
            from: u32,
            entry: Edge,
            right: P,
            left: P,
        }
        // In front of `r`: needed once a cone narrows to a single ray.
        let unit = |v: P| {
            let len = v[0].hypot(v[1]);
            if len == 0.0 { v } else { [v[0] / len, v[1] / len] }
        };
        let ahead = |c: &Cone, v: P| {
            let (a, b) = (unit(sub(c.right, r)), unit(sub(c.left, r)));
            let d = sub(v, r);
            (a[0] + b[0]) * d[0] + (a[1] + b[1]) * d[1] > 0.0
        };
        let in_cone = |c: &Cone, v: P| side(r, c.right, v) >= -TOL && side(r, c.left, v) <= TOL && ahead(c, v);
        let mut stack = Vec::new();
        for &t in incident {
            // The trapezoids holding `r` are convex: all of each is visible.
            for &c in self.trap_corners(t) {
                seen(Some(c));
            }
            if goal_traps.contains(&t) {
                seen(None);
            }
            for g in self.gates(t) {
                if incident.contains(&g.to) {
                    continue;
                }
                // `r` on the gate's line: nothing beyond is visible through it.
                let c = cross(sub(g.a, r), sub(g.b, r));
                if c.abs() <= TOL * dist(g.a, g.b) {
                    continue;
                }
                let (right, left) = if c > 0.0 { (g.a, g.b) } else { (g.b, g.a) };
                stack.push(Cone { trap: g.to, from: t, entry: g.edge.across(), right, left });
            }
        }
        let mut steps = 0;
        while let Some(cone) = stack.pop() {
            steps += 1;
            if steps > MAX_CONE_STEPS {
                break;
            }
            for &c in self.trap_corners(cone.trap) {
                if in_cone(&cone, self.corners[c as usize].p) {
                    seen(Some(c));
                }
            }
            if goal_traps.contains(&cone.trap) && in_cone(&cone, goal) {
                seen(None);
            }
            for g in self.gates(cone.trap) {
                // Rays leave through another side, and never re-enter a
                // (convex) trapezoid they have left.
                if g.edge == cone.entry || g.to == cone.from || incident.contains(&g.to) {
                    continue;
                }
                if !ahead(&cone, g.a) && !ahead(&cone, g.b) {
                    continue;
                }
                let c = cross(sub(g.a, r), sub(g.b, r));
                if c.abs() <= TOL * dist(g.a, g.b) {
                    continue;
                }
                let (p, q) = if c > 0.0 { (g.a, g.b) } else { (g.b, g.a) };
                // Wholly right or wholly left of the cone.
                if side(r, cone.right, q) < -TOL || side(r, cone.left, p) > TOL {
                    continue;
                }
                // Clip the gate p→q (right to left as seen from `r`) to the cone.
                let hit = |d: P| {
                    let dir = sub(d, r);
                    let den = cross(dir, sub(q, p));
                    (den.abs() > f64::EPSILON).then(|| cross(dir, sub(r, p)) / den)
                };
                let lo = if side(r, cone.right, p) >= 0.0 {
                    0.0
                } else {
                    match hit(cone.right) {
                        Some(t) => t.max(0.0),
                        None => continue,
                    }
                };
                let hi = if side(r, cone.left, q) <= 0.0 {
                    1.0
                } else {
                    match hit(cone.left) {
                        Some(t) => t.min(1.0),
                        None => continue,
                    }
                };
                // A cone narrowed to a single point would only graze a vertex
                // (and could circle the trapezoids around it); the corner
                // there, if any, has been seen already.
                if (hi - lo) * dist(p, q) <= MIN_GAP {
                    continue;
                }
                let (right, left) = (lerp(p, q, lo), lerp(p, q, hi));
                stack.push(Cone { trap: g.to, from: cone.trap, entry: g.edge.across(), right, left });
            }
        }
    }

    /// The shortest path from `from` to `to`. Each is placed on its own
    /// plane if that plane holds it, else on the highest-numbered plane that
    /// does.
    pub fn route(&self, from: Waypoint, to: Waypoint) -> Result<Route, RouteError> {
        let start: P = [from.x.into(), from.y.into()];
        let goal: P = [to.x.into(), to.y.into()];
        let start_trap = self.locate(start, from.plane).ok_or(RouteError::StartOffMesh)?;
        let goal_trap = self.locate(goal, to.plane).ok_or(RouteError::GoalOffMesh)?;
        let goal_traps = self.incident(goal, goal_trap);

        // Nodes: the corners, then the start and the goal.
        let n = self.corners.len();
        let (start_node, goal_node) = (n, n + 1);
        let pos = |v: usize| match v {
            v if v == start_node => start,
            v if v == goal_node => goal,
            v => self.corners[v].p,
        };
        let mut g = vec![f64::INFINITY; n + 2];
        let mut parent = vec![usize::MAX; n + 2];
        let mut closed = vec![false; n + 2];
        let mut open = BinaryHeap::new();
        g[start_node] = 0.0;
        open.push((Reverse(Cost(dist(start, goal))), start_node));
        while let Some((_, u)) = open.pop() {
            if closed[u] {
                continue;
            }
            closed[u] = true;
            if u == goal_node {
                break;
            }
            let pu = pos(u);
            let seed = if u == start_node { start_trap } else { self.corners[u].trap };
            let incident = self.incident(pu, seed);
            self.visible(pu, &incident, goal, &goal_traps, |v| {
                let v = v.map_or(goal_node, |c| c as usize);
                if closed[v] {
                    return;
                }
                let cost = g[u] + dist(pu, pos(v));
                if cost < g[v] {
                    g[v] = cost;
                    parent[v] = u;
                    open.push((Reverse(Cost(cost + dist(pos(v), goal))), v));
                }
            });
        }
        if !closed[goal_node] {
            return Err(RouteError::Unreachable);
        }

        let mut nodes = vec![goal_node];
        while let Some(&last) = nodes.last()
            && last != start_node
        {
            nodes.push(parent[last]);
        }
        nodes.reverse();
        // Drop bends that are no bend (ties with the straight line).
        let mut i = 1;
        while i + 1 < nodes.len() {
            if side(pos(nodes[i - 1]), pos(nodes[i + 1]), pos(nodes[i])).abs() < TOL {
                nodes.remove(i);
            } else {
                i += 1;
            }
        }
        let points = nodes
            .iter()
            .map(|&v| match v {
                v if v == start_node => from,
                v if v == goal_node => to,
                v => {
                    let c = &self.corners[v];
                    Waypoint { x: c.p[0] as f32, y: c.p[1] as f32, plane: c.plane }
                }
            })
            .collect();
        Ok(Route { points, length: g[goal_node] as f32 })
    }
}

/// A path cost, ordered for the open list.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cost(f64);

impl Eq for Cost {}

impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::navmesh::{NONE_U16, NodeRef, Portal};

    /// A trapezoid with its neighbours above (`up`) and below (`down`).
    fn trap(y: [f32; 2], top: [f32; 2], bottom: [f32; 2], up: &[u32], down: &[u32]) -> Trapezoid {
        let mut neighbors = [NONE_U32; 4];
        neighbors[..up.len()].copy_from_slice(up);
        neighbors[2..2 + down.len()].copy_from_slice(down);
        Trapezoid {
            neighbors,
            portal_left: NONE_U16,
            portal_right: NONE_U16,
            y_top: y[1],
            y_bottom: y[0],
            x_top_left: top[0],
            x_top_right: top[1],
            x_bottom_left: bottom[0],
            x_bottom_right: bottom[1],
        }
    }

    fn plane(trapezoids: Vec<Trapezoid>, portals: Vec<Portal>, portal_trapezoids: Vec<u32>) -> PathPlane {
        PathPlane {
            start_points: vec![],
            vectors: vec![],
            trapezoids,
            root: NodeRef::None,
            x_nodes: vec![],
            y_nodes: vec![],
            sinks: vec![],
            portal_trapezoids,
            portals,
        }
    }

    fn wp(x: f32, y: f32, plane: u32) -> Waypoint {
        Waypoint { x, y, plane }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// A 10×10 room with a 2-wide wall rising from the bottom to y = 8.
    fn room() -> NavMesh {
        NavMesh::new(&[plane(
            vec![
                trap([0.0, 8.0], [0.0, 4.0], [0.0, 4.0], &[2], &[]),
                trap([0.0, 8.0], [6.0, 10.0], [6.0, 10.0], &[2], &[]),
                trap([8.0, 10.0], [0.0, 10.0], [0.0, 10.0], &[], &[0, 1]),
            ],
            vec![],
            vec![],
        )])
    }

    #[test]
    fn bends_around_a_wall() {
        let mesh = room();
        let route = mesh.route(wp(1.0, 1.0, 0), wp(9.0, 1.0, 0)).unwrap();
        let points: Vec<[f32; 2]> = route.points.iter().map(|w| [w.x, w.y]).collect();
        assert_eq!(points, vec![[1.0, 1.0], [4.0, 8.0], [6.0, 8.0], [9.0, 1.0]]);
        assert!(close(route.length, 2.0 * 58f32.sqrt() + 2.0));
    }

    #[test]
    fn straight_when_visible() {
        let mesh = room();
        let route = mesh.route(wp(1.0, 1.0, 0), wp(9.0, 9.0, 0)).unwrap();
        // Through the top strip: the wall's top-left corner is in the way.
        assert_eq!(route.points.len(), 3);
        assert_eq!((route.points[1].x, route.points[1].y), (4.0, 8.0));
        let direct = mesh.route(wp(0.5, 7.5, 0), wp(9.5, 9.5, 0)).unwrap();
        assert_eq!(direct.points.len(), 2);
        let same = mesh.route(wp(1.0, 1.0, 0), wp(2.0, 3.0, 0)).unwrap();
        assert_eq!(same.points.len(), 2);
        assert!(close(same.length, 5f32.sqrt()));
    }

    #[test]
    fn crosses_a_portal_between_planes() {
        // Plane 0: x 0..4, y 0..10, right side on portal id 7. Plane 1:
        // x 4..8, y 6..10, left side on the same portal.
        let mut t = trap([0.0, 10.0], [0.0, 4.0], [0.0, 4.0], &[], &[]);
        t.portal_right = 0;
        let mut u = trap([6.0, 10.0], [4.0, 8.0], [4.0, 8.0], &[], &[]);
        u.portal_left = 0;
        let portal = |to| Portal { count: 1, start: 0, neighbor_plane: to, pair: 7, flags: 0 };
        let mesh = NavMesh::new(&[plane(vec![t], vec![portal(1)], vec![0]), plane(vec![u], vec![portal(0)], vec![0])]);
        let route = mesh.route(wp(1.0, 1.0, 0), wp(7.0, 9.0, 1)).unwrap();
        let points: Vec<[f32; 2]> = route.points.iter().map(|w| [w.x, w.y]).collect();
        assert_eq!(points, vec![[1.0, 1.0], [4.0, 6.0], [7.0, 9.0]]);
        assert_eq!(route.points.last().unwrap().plane, 1);
        // Straight through the portal.
        assert_eq!(mesh.route(wp(1.0, 7.0, 0), wp(7.0, 9.0, 1)).unwrap().points.len(), 2);
        // Without the portal the planes are disconnected.
        let no_portal = NavMesh::new(&[
            plane(vec![trap([0.0, 10.0], [0.0, 4.0], [0.0, 4.0], &[], &[])], vec![], vec![]),
            plane(vec![trap([6.0, 10.0], [4.0, 8.0], [4.0, 8.0], &[], &[])], vec![], vec![]),
        ]);
        assert_eq!(no_portal.route(wp(1.0, 1.0, 0), wp(7.0, 9.0, 1)), Err(RouteError::Unreachable));
    }

    #[test]
    fn endpoints_must_be_on_a_plane() {
        let mesh = room();
        assert_eq!(mesh.route(wp(5.0, 1.0, 0), wp(9.0, 1.0, 0)), Err(RouteError::StartOffMesh));
        assert_eq!(mesh.route(wp(1.0, 1.0, 0), wp(11.0, 1.0, 0)), Err(RouteError::GoalOffMesh));
        // A plane that doesn't hold the point falls back to one that does.
        assert!(mesh.route(wp(1.0, 1.0, 3), wp(9.0, 1.0, 0)).is_ok());
        assert_eq!(mesh.plane_at([1.0, 1.0], 3), Some(0));
    }

    /// Dijkstra over trapezoids through gate midpoints: a valid (not
    /// shortest) path length, or `None` if unreachable.
    fn gate_path_length(mesh: &NavMesh, from: u32, to: u32, start: P, goal: P) -> Option<f64> {
        let mut best: HashMap<u32, (f64, P)> = HashMap::from([(from, (0.0, start))]);
        let mut open = BinaryHeap::from([(Reverse(Cost(0.0)), from)]);
        while let Some((Reverse(Cost(d)), t)) = open.pop() {
            let (bd, at) = best[&t];
            if d > bd {
                continue;
            }
            if t == to {
                return Some(d + dist(at, goal));
            }
            for g in mesh.gates(t) {
                let mid = lerp(g.a, g.b, 0.5);
                let nd = d + dist(at, mid);
                if best.get(&g.to).is_none_or(|&(od, _)| nd < od) {
                    best.insert(g.to, (nd, mid));
                    open.push((Reverse(Cost(nd)), g.to));
                }
            }
        }
        None
    }

    /// On the sample maps, routes between trapezoid centres stay on the
    /// mesh, are never longer than the gate-midpoint path and exist exactly
    /// when that path does.
    #[test]
    fn routes_on_sample_maps() {
        for f in crate::pathgen::testing::Fixture::all() {
            let mesh = NavMesh::new(&f.client_planes);
            let full = NavMesh::build(&f.client_planes, false);
            let center = |i: u32| {
                let t = &mesh.traps[i as usize];
                [(t.x_top_left + t.x_top_right + t.x_bottom_left + t.x_bottom_right) / 4.0, (t.y_top + t.y_bottom) / 2.0]
            };
            // Within 0.05 of a trapezoid (perpendicular distance).
            let on_mesh = |p: P| {
                mesh.traps.iter().filter(|t| t.contains(p, 1.0)).any(|t| {
                    let c = [
                        [t.x_top_left, t.y_top],
                        [t.x_top_right, t.y_top],
                        [t.x_bottom_right, t.y_bottom],
                        [t.x_bottom_left, t.y_bottom],
                    ];
                    t.contains(p, 0.0) || (0..4).any(|i| segment_distance(p, c[i], c[(i + 1) % 4]) <= 0.05)
                })
            };
            let mut seed = 0x2545_f491_u64;
            let mut next = |n: usize| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed % n as u64) as u32
            };
            let (mut found, mut unreachable, mut bends, mut straight) = (0, 0, 0, 0);
            let mut searching = std::time::Duration::ZERO;
            let mut slowest = std::time::Duration::ZERO;
            // The checks are slow unoptimized.
            let pairs = if cfg!(debug_assertions) { 15 } else { 60 };
            for _ in 0..pairs {
                let (a, b) = (next(mesh.traps.len()), next(mesh.traps.len()));
                if mesh.traps[a as usize].y_top <= mesh.traps[a as usize].y_bottom
                    || mesh.traps[b as usize].y_top <= mesh.traps[b as usize].y_bottom
                {
                    continue;
                }
                let (pa, pb) = (center(a), center(b));
                let from = wp(pa[0] as f32, pa[1] as f32, mesh.traps[a as usize].plane);
                let to = wp(pb[0] as f32, pb[1] as f32, mesh.traps[b as usize].plane);
                let (ta, tb) = (mesh.locate(pa, from.plane).unwrap(), mesh.locate(pb, to.plane).unwrap());
                let baseline = gate_path_length(&mesh, ta, tb, pa, pb);
                let started = std::time::Instant::now();
                let result = mesh.route(from, to);
                searching += started.elapsed();
                slowest = slowest.max(started.elapsed());
                match result {
                    Ok(route) => {
                        found += 1;
                        bends += route.points.len() - 2;
                        for w in route.points.windows(3) {
                            let (a, b, c) = ([w[0].x as f64, w[0].y as f64], [w[1].x as f64, w[1].y as f64], [w[2].x as f64, w[2].y as f64]);
                            if side(a, c, b).abs() < 0.01 {
                                straight += 1;
                            }
                        }
                        let baseline = baseline.expect("route found but no gate path");
                        let unpruned = full.route(from, to).unwrap();
                        assert!((route.length - unpruned.length).abs() < 0.01, "pruned {} vs {}", route.length, unpruned.length);
                        assert!(route.length as f64 <= baseline + 0.1, "{} > {baseline}", route.length);
                        assert!(route.length as f64 >= dist(pa, pb) - 0.1);
                        for w in route.points.windows(2) {
                            let (p, q) = ([w[0].x as f64, w[0].y as f64], [w[1].x as f64, w[1].y as f64]);
                            let steps = (dist(p, q) / 4.0).ceil().max(1.0) as usize;
                            for k in 0..=steps {
                                let s = lerp(p, q, k as f64 / steps as f64);
                                assert!(on_mesh(s), "{:?}: segment {p:?} -> {q:?} leaves the mesh at {s:?}", f.pair);
                            }
                        }
                    }
                    Err(RouteError::Unreachable) => {
                        unreachable += 1;
                        assert!(baseline.is_none(), "gate path exists but no route");
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            eprintln!(
                "{:?}: {} corners (of {}), {found} routes ({bends} bends, {straight} straight), {unreachable} unreachable, search {searching:.2?} (slowest {slowest:.2?})",
                f.pair,
                mesh.corners.len(),
                full.corners.len(),
            );
        }
    }

    #[test]
    fn uncovered_ranges() {
        assert_eq!(uncovered(0.0, 10.0, &mut [(2.0, 4.0), (6.0, 10.0)]), vec![(0.0, 2.0), (4.0, 6.0)]);
        assert_eq!(uncovered(0.0, 10.0, &mut [(0.0, 10.0)]), vec![]);
        assert_eq!(uncovered(3.0, 3.0, &mut []), vec![(3.0, 3.0)]);
    }
}
