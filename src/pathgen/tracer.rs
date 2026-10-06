//! The terrain tracer: `PathTracer_Init` and `Engine\Map\Path\PathFlood.cpp`.
//!
//! Builds a triangle grid from the heights, classifies each triangle by
//! slope, floods the walkable area from each start point, traces the
//! flooded region's outline, simplifies it and turns it into segments.

use std::collections::HashMap;

use super::X87;
use super::fastmath;
use super::random::Random;

/// World units between height samples.
const XY: f64 = 96.0;

pub mod flag {
    /// Not walkable (too steep, underwater, or the grid border).
    pub const BLOCKED: u32 = 0x1;
    /// Steep: only entered from a triangle of similar slope.
    pub const STEEP: u32 = 0x2;
    pub const VISITED: u32 = 0x4;
    /// A start point or portal cell; the outline may not be simplified
    /// across it.
    pub const SEED: u32 = 0x10;
    /// Within two cells of a start point or portal.
    pub const NEAR_SEED: u32 = 0x20;
    /// Marks the end of a polygon in the vertex list.
    pub const POLYGON_END: u32 = 0x40;
}

#[derive(Debug, Clone, Copy)]
pub struct TracerInput<'a> {
    /// Selects the slope limits; `< 2` is the gentler set.
    pub map_type: u32,
    /// Triangles whose first vertex has `z >= 0` count as walkable water.
    pub water: bool,
    /// Height samples along x and y.
    pub dims: [usize; 2],
    /// Row-major heights; row 0 is at `max_y`.
    pub heights: &'a [f32],
    pub min_x: f32,
    pub max_y: f32,
    pub start_points: &'a [[f32; 2]],
    /// Pairs of points; the cells along each line are forced walkable.
    pub portal_points: &'a [[f32; 2]],
    pub x87: X87,
}

/// Slope limits in radians: `[steep, blocked, flood_delta]`, i.e.
/// 30/35/15 degrees, or 40/45/10 degrees for map types 2 and up.
fn limits(map_type: u32) -> [f32; 3] {
    let bits = if map_type < 2 {
        [0x3F06_0A92, 0x3F1C_61AB, 0x3E86_0A92]
    } else {
        [0x3F32_B8C3, 0x3F49_0FDB, 0x3E32_B8C3]
    };
    bits.map(f32::from_bits)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tri {
    angle: f32,
    flags: u32,
}

/// Cells are `w * h`; each holds two triangles. Triangle `t` of cell
/// `(c, r)` is `2 * (r * w + c) + half`. Half 0 is the upper-left triangle
/// `(v00, v01, v10)`, half 1 the lower-right `(v01, v11, v10)`.
struct Grid {
    w: usize,
    h: usize,
    tris: Vec<Tri>,
    /// World position of grid corner (0, 0).
    x0: f32,
    y_top: f32,
}

impl Grid {
    fn cell(&mut self, c: usize, r: usize) -> &mut [Tri] {
        let i = 2 * (r * self.w + c);
        &mut self.tris[i..i + 2]
    }

    fn visited(&self, t: isize) -> bool {
        self.tris[t as usize].flags & flag::VISITED != 0
    }
}

/// `Terrain_compute_grid_cell`: classify triangle (a, b, c).
fn classify(x87: X87, limits: &[f32; 3], water: bool, a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Tri {
    let r = |v: f64| x87.r(v);
    let s = |v: f64| x87.f32(v) as f64;
    let e1z = s(a[2] as f64 - b[2] as f64);
    let e2y = s(c[1] as f64 - b[1] as f64);
    let e1y = s(a[1] as f64 - b[1] as f64);
    let e2z = s(c[2] as f64 - b[2] as f64);
    let e1x = s(a[0] as f64 - b[0] as f64);
    let e2x = s(c[0] as f64 - b[0] as f64);
    let nz = s(r(e2x * e1y) - r(e1x * e2y));
    let ny = s(r(e1x * e2z) - r(e2x * e1z));
    let nx = s(r(e1z * e2y) - r(e1y * e2z));
    let len2 = x87.f32(r(r(nx * nx) + r(ny * ny)) + r(nz * nz));
    let nzn = x87.f32(nz * fastmath::rsqrt(len2) as f64);
    let angle = (-(nzn as f64)).acos();

    let flags = if water && a[2] >= 0.0 {
        0
    } else if (a[2] > 40.0 && b[2] > 40.0 && c[2] > 40.0) || (limits[1] as f64) < angle {
        flag::BLOCKED
    } else if (limits[0] as f64) < angle {
        flag::STEEP
    } else {
        0
    };
    Tri { angle: angle as f32, flags }
}

/// A traced polygon vertex: world position and the flags of the triangle
/// it was traced from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub flags: u32,
}

/// A 56-byte `PathFlood` segment. `p0` is the upper end (larger y, then
/// larger x).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub p0: [f64; 2],
    pub p1: [f64; 2],
    pub vector: [f64; 2],
}

impl Segment {
    fn new(a: Vertex, b: Vertex) -> Self {
        let (hi, lo) = if a.y > b.y || (a.y == b.y && b.x < a.x) { (a, b) } else { (b, a) };
        Self {
            p0: [hi.x as f64, hi.y as f64],
            p1: [lo.x as f64, lo.y as f64],
            vector: [lo.x as f64 - hi.x as f64, (lo.y - hi.y) as f64],
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TracerOutput {
    /// Simplified polygons in trace order (closed: last == first).
    pub polygons: Vec<Vec<Vertex>>,
    /// All polygon edges, shuffled as the client does.
    pub segments: Vec<Segment>,
}

pub fn trace(input: &TracerInput) -> TracerOutput {
    let x87 = input.x87;
    let limits = limits(input.map_type);
    let [dim_x, dim_y] = input.dims;
    assert_eq!(input.heights.len(), dim_x * dim_y);
    let (w, h) = (dim_x + 2, dim_y + 2);
    let x0 = x87.f32(input.min_x as f64 - XY);
    let y_top = x87.f32(input.max_y as f64 + XY);
    let x1 = x87.f32(x87.r(w as f64 * XY) + x0 as f64);
    let y_bottom = x87.f32(y_top as f64 - x87.r(h as f64 * XY));
    let mut grid = Grid { w, h, tris: vec![Tri::default(); w * h * 2], x0, y_top };

    // Terrain_generate_grid_vertices.
    let height = |x: usize, y: usize| input.heights[y * dim_x + x];
    for r in 0..dim_y - 1 {
        for c in 0..dim_x - 1 {
            let v00 = [0.0, 0.0, height(c, r)];
            let v01 = [96.0, 0.0, height(c + 1, r)];
            let v10 = [0.0, -96.0, height(c, r + 1)];
            let v11 = [96.0, -96.0, height(c + 1, r + 1)];
            let a = classify(x87, &limits, input.water, v00, v01, v10);
            let b = classify(x87, &limits, input.water, v01, v11, v10);
            grid.cell(c + 1, r + 1).copy_from_slice(&[a, b]);
        }
        let last = [grid.cell(dim_x - 1, r + 1)[0], grid.cell(dim_x - 1, r + 1)[1]];
        grid.cell(dim_x, r + 1).copy_from_slice(&last);
    }
    for c in 1..=dim_x {
        let above = [grid.cell(c, dim_y - 1)[0], grid.cell(c, dim_y - 1)[1]];
        grid.cell(c, dim_y).copy_from_slice(&above);
    }

    let rect = LineRect { x0, y_top, w, h, x87 };
    mark_portals(&mut grid, &rect, input.portal_points);

    // Mark the start points (PathFlood_mark_blocked_cells).
    for p in input.start_points {
        let yc = (y_bottom.max(p[1])).min(y_top);
        let xc = (x0.max(p[0])).min(x1);
        let cy = floor_u(x87.f32(x87.r(y_top as f64 - yc as f64) / XY));
        let cx = floor_u(x87.f32(x87.r(xc as f64 - x0 as f64) / XY));
        mark_cell_block(&mut grid, cx, cy);
    }

    // Terrain_init_grid_edges.
    let border = Tri { angle: f32::from_bits(0x3FC9_0FDB), flags: flag::BLOCKED };
    for c in 0..w {
        grid.cell(c, 0).fill(border);
        grid.cell(c, h - 1).fill(border);
    }
    for r in 0..h {
        grid.cell(0, r).fill(border);
        grid.cell(w - 1, r).fill(border);
    }

    let mut out = TracerOutput::default();
    for p in input.start_points {
        let (tri, row) = world_to_grid(x87, x0, y_top, *p);
        flood(&mut grid, row * 2 * w + tri, limits[2]);
        let edges = collect_edges(&grid);
        let polygons = trace_contours(&grid, edges, &rect, x87);
        for poly in polygons {
            out.segments.extend(poly.windows(2).map(|e| Segment::new(e[0], e[1])));
            out.polygons.push(poly);
        }
    }
    Random::new(0).shuffle(&mut out.segments);
    out
}

/// `ClampFloat`: floor of a non-negative value.
fn floor_u(v: f32) -> usize {
    assert!(v >= 0.0, "ClampFloat: {v} < 0");
    v.floor() as usize
}

/// Set `NEAR_SEED` on the 5x5 cells around `(cx, cy)` and make the cell
/// itself a walkable seed cell.
fn mark_cell_block(grid: &mut Grid, cx: usize, cy: usize) {
    let (w, h) = (grid.w, grid.h);
    for r in cy.saturating_sub(2)..(cy + 3).min(h) {
        for c in cx.saturating_sub(2)..(cx + 3).min(w) {
            for t in grid.cell(c, r) {
                t.flags |= flag::NEAR_SEED;
            }
        }
    }
    if cx < w && cy < h {
        for t in grid.cell(cx, cy) {
            t.flags = flag::SEED | flag::NEAR_SEED;
        }
    }
}

/// `PathFlood_MarkPolygon`: every cell along each portal line becomes a
/// seed cell.
fn mark_portals(grid: &mut Grid, rect: &LineRect, points: &[[f32; 2]]) {
    assert!(points.len().is_multiple_of(2), "pointCount % 2 == 0");
    for pair in points.chunks_exact(2) {
        for (c, r) in rect.cells_on_line(pair[0], pair[1]) {
            mark_cell_block(grid, c, r);
        }
    }
}

/// `PathFlood_world_to_grid`: triangle column (`2 * cell + half`) and row.
fn world_to_grid(x87: X87, x0: f32, y_top: f32, p: [f32; 2]) -> (usize, usize) {
    let fx = x87.f32(x87.r(p[0] as f64 - x0 as f64) / XY);
    let fy = x87.f32(x87.r(y_top as f64 - p[1] as f64) / XY);
    let (ix, iy) = (floor_u(fx), floor_u(fy));
    let frac = x87.r(x87.r(fx as f64 - ix as f64) + x87.r(fy as f64 - iy as f64));
    (2 * ix + (frac > 1.0) as usize, iy)
}

/// The first part of `PathFlood_trace_edges`: flood from triangle `seed`.
fn flood(grid: &mut Grid, seed: usize, delta: f32) {
    if grid.tris[seed].flags & flag::BLOCKED != 0 {
        return;
    }
    let row = 2 * grid.w as isize;
    let mut queue = std::collections::VecDeque::from([seed as isize]);
    while let Some(t) = queue.pop_front() {
        let cur = grid.tris[t as usize];
        if cur.flags & flag::VISITED != 0 {
            continue;
        }
        grid.tris[t as usize].flags |= flag::VISITED;
        let third = if t & 1 == 1 { t + row - 1 } else { t - row + 1 };
        for n in [t - 1, t + 1, third] {
            let nb = grid.tris[n as usize];
            if nb.flags & (flag::BLOCKED | flag::VISITED) != 0 {
                continue;
            }
            if nb.flags & flag::STEEP != 0 && (cur.angle - nb.angle).abs() >= delta {
                continue;
            }
            queue.push_back(n);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EdgeKey([u32; 4]);

#[derive(Debug, Clone, Copy)]
struct Edge {
    key: EdgeKey,
    kind: usize,
    tri: isize,
}

/// Boundary edges in insertion order, removable by key (the client's
/// `THash` with its insertion-ordered full list).
struct EdgeTable {
    edges: Vec<Option<Edge>>,
    index: HashMap<EdgeKey, usize>,
    head: usize,
}

impl EdgeTable {
    fn push(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, kind: usize, tri: usize) {
        let key = EdgeKey([x0 as u32, y0 as u32, x1 as u32, y1 as u32]);
        self.index.insert(key, self.edges.len());
        self.edges.push(Some(Edge { key, kind, tri: tri as isize }));
    }

    fn take(&mut self, key: EdgeKey) -> Option<Edge> {
        let i = self.index.remove(&key)?;
        self.edges[i].take()
    }

    fn take_first(&mut self) -> Option<Edge> {
        while self.head < self.edges.len() {
            if let Some(e) = self.edges[self.head].take() {
                self.index.remove(&e.key);
                return Some(e);
            }
            self.head += 1;
        }
        None
    }
}

/// The second part of `PathFlood_trace_edges`: directed edges between
/// visited and unvisited triangles. All half-0 edges come first, then all
/// half-1 edges, each in row-major cell order.
fn collect_edges(grid: &Grid) -> EdgeTable {
    let mut table = EdgeTable { edges: Vec::new(), index: HashMap::new(), head: 0 };
    let (w, h) = (grid.w, grid.h);
    let row = 2 * w;
    let visited = |t: usize| grid.tris[t].flags & flag::VISITED != 0;
    for r in 1..h - 1 {
        for c in 1..w - 1 {
            let t = 2 * (r * w + c);
            if !visited(t) {
                continue;
            }
            if !visited(t + 1) {
                table.push(c + 1, r, c, r + 1, 5, t);
            }
            if !visited(t - 1) {
                table.push(c, r + 1, c, r, 2, t);
            }
            if !visited(t - row + 1) {
                table.push(c, r, c + 1, r, 0, t);
            }
        }
    }
    for r in 1..h - 1 {
        for c in 1..w - 1 {
            let t = 2 * (r * w + c) + 1;
            if !visited(t) {
                continue;
            }
            if !visited(t + 1) {
                table.push(c + 1, r, c + 1, r + 1, 3, t);
            }
            if !visited(t - 1) {
                table.push(c, r + 1, c + 1, r, 4, t);
            }
            if !visited(t + row - 1) {
                table.push(c + 1, r + 1, c, r + 1, 1, t);
            }
        }
    }
    table
}

/// Next edge type, per current type and turn (`0xbfa390`).
const NEXT_KIND: [[usize; 5]; 6] = [
    [5, 3, 0, 4, 2],
    [4, 2, 1, 5, 3],
    [0, 4, 2, 1, 5],
    [1, 5, 3, 0, 4],
    [3, 0, 4, 2, 1],
    [2, 1, 5, 3, 0],
];
/// Grid step of the next edge (`0xbfa408` and `0xbfa480`).
const STEP_X: [[i32; 5]; 6] = [
    [-1, 0, 1, 1, 0],
    [1, 0, -1, -1, 0],
    [1, 1, 0, -1, -1],
    [-1, -1, 0, 1, 1],
    [0, 1, 1, 0, -1],
    [0, -1, -1, 0, 1],
];
const STEP_Y: [[i32; 5]; 6] = [
    [1, 1, 0, -1, -1],
    [-1, -1, 0, 1, 1],
    [0, -1, -1, 0, 1],
    [0, 1, 1, 0, -1],
    [1, 0, -1, -1, 0],
    [-1, 0, 1, 1, 0],
];

/// Triangle offsets around the end vertex of an edge, per edge type.
fn turn_offsets(w: usize) -> [[isize; 6]; 6] {
    let r = 2 * w as isize;
    [
        [0, 1, 2, -r + 3, -r + 2, -r + 1],
        [0, -1, -2, r - 3, r - 2, r - 1],
        [0, -r + 1, -r, -r - 1, -2, -1],
        [0, r - 1, r, r + 1, 2, 1],
        [0, 1, -r + 2, -r + 1, -r, -1],
        [0, -1, r - 2, r - 1, r, 1],
    ]
}

/// `PathFlood_trace_contours`: walk every boundary, starting from the
/// oldest remaining edge, and simplify each polygon.
fn trace_contours(grid: &Grid, mut table: EdgeTable, rect: &LineRect, x87: X87) -> Vec<Vec<Vertex>> {
    let offsets = turn_offsets(grid.w);
    let mut polygons = Vec::new();
    while let Some(first) = table.take_first() {
        let [fx0, fy0, fx1, fy1] = first.key.0;
        let flags = |t: isize| grid.tris[t as usize].flags;
        let mut points = vec![(fx0, fy0, flags(first.tri))];
        let (mut x, mut y, mut kind, mut tri) = (fx1, fy1, first.kind, first.tri);
        loop {
            points.push((x, y, flags(tri)));
            let turn = (0..5)
                .find(|&k| !grid.visited(tri + offsets[kind][k + 1]))
                .expect("No adjacent edge");
            tri += offsets[kind][turn];
            let nx = (x as i32 + STEP_X[kind][turn]) as u32;
            let ny = (y as i32 + STEP_Y[kind][turn]) as u32;
            kind = NEXT_KIND[kind][turn];
            let key = EdgeKey([x, y, nx, ny]);
            if key == first.key {
                break;
            }
            table.take(key).expect("tableEdge");
            (x, y) = (nx, ny);
        }
        let mut verts: Vec<Vertex> = points
            .into_iter()
            .map(|(gx, gy, flags)| Vertex {
                x: x87.f32(grid.x0 as f64 + x87.r(gx as f64 * XY)),
                y: x87.f32(grid.y_top as f64 - x87.r(gy as f64 * XY)),
                flags,
            })
            .collect();
        let n = simplify(grid, rect, x87, &mut verts);
        if n != 0 {
            verts.truncate(n);
            polygons.push(verts);
        }
    }
    polygons
}

/// `PathFlood_simplify_polygon`. Returns the new vertex count, or 0 if
/// fewer than 4 vertices remain.
fn simplify(grid: &Grid, rect: &LineRect, x87: X87, v: &mut [Vertex]) -> usize {
    let n = v.len();
    if n <= 2 {
        return 0;
    }
    let r = |x: f64| x87.r(x);
    let s = |x: f64| x87.f32(x) as f64;

    // Pass 1: greedily extend each output edge over as many vertices as
    // stay close to it, allowing up to 6 failures in a row.
    let mut dest = 1;
    let mut anchor = 0;
    if 1 < n {
        loop {
            let mut fails = 0;
            let mut best = anchor + 1;
            for cand in anchor + 2..n {
                if simplify_ok(grid, rect, x87, v, anchor, cand) {
                    fails = 0;
                    best = cand;
                } else {
                    fails += 1;
                    if fails == 7 {
                        break;
                    }
                }
            }
            let b = v[best];
            let last = v[dest - 1];
            if b.x != last.x || b.y != last.y {
                let collinear = dest > 1 && {
                    let prev = v[dest - 2];
                    let cross = s(r(s(last.y as f64 - b.y as f64) * s(prev.x as f64 - last.x as f64))
                        - r(s(last.x as f64 - b.x as f64) * s(prev.y as f64 - last.y as f64)));
                    cross == 0.0
                };
                if collinear {
                    v[dest - 1] = b;
                } else {
                    v[dest] = b;
                    dest += 1;
                }
                if dest > 1 {
                    let (a, c) = (v[dest - 1], v[dest - 2]);
                    assert!(a.x != c.x || a.y != c.y, "(dest == vertices + 1) || (dest[-1].pos != dest[-2].pos)");
                }
            }
            anchor = best;
            if anchor + 1 >= n {
                break;
            }
        }
    }

    let n = dest;
    if n < 4 {
        return 0;
    }

    // Pass 2: straighten corners that are close to straight, or short and
    // up to 45 degrees off, by projecting onto the longer edge.
    let mut prev = 0;
    for cur in 1..n - 1 {
        let next = if cur + 1 == n - 1 { 0 } else { cur + 1 };
        let (p, c, q) = (v[prev], v[cur], v[next]);
        let ax = s(p.x as f64 - c.x as f64);
        let ay = s(p.y as f64 - c.y as f64);
        let bx = s(q.x as f64 - c.x as f64);
        let by = s(q.y as f64 - c.y as f64);
        let dot = s(r(r(bx * ax) + r(ay * by)));
        if dot > 0.0 {
            let la = s(r(r(ax * ax) + r(ay * ay)));
            let lb = s(r(r(by * by) + r(bx * bx)));
            let cos2 = s(r(r(dot * dot) / r(la * lb)));
            let short = la <= 18496.0 || lb <= 18496.0;
            if cos2 > 0.75 || (cos2 > 0.49999997f32 as f64 && short) {
                let (ex, ey, len) = if lb < la { (ax, ay, la) } else { (bx, by, lb) };
                let t = s(r(dot / len));
                let oy = s(r(ey * t));
                let ny = x87.f32(r(oy + c.y as f64));
                let ox = s(r(ex * t));
                let nx = x87.f32(r(ox + c.x as f64));
                v[cur].x = super::round(nx) as f32;
                v[cur].y = super::round(ny) as f32;
            }
        }
        prev = cur;
    }

    // Drop consecutive duplicates.
    let mut kept = 1;
    for i in 1..n {
        if v[i].x != v[kept - 1].x || v[i].y != v[kept - 1].y {
            v[kept] = v[i];
            kept += 1;
        }
    }
    if kept < 4 { 0 } else { kept }
}

/// Can vertices `anchor + 1 .. cand` be dropped in favour of the edge
/// `anchor -> cand`?
// The negated comparisons mirror the client's x87 branches, NaN included.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn simplify_ok(grid: &Grid, rect: &LineRect, x87: X87, v: &[Vertex], anchor: usize, cand: usize) -> bool {
    let r = |x: f64| x87.r(x);
    let s = |x: f64| x87.f32(x) as f64;
    let a = v[anchor];
    let dx = s(v[cand].x as f64 - a.x as f64);
    let dy = s(v[cand].y as f64 - a.y as f64);
    let mut acc = 0.0f64;
    for mid in &v[anchor + 1..cand] {
        let px = s(mid.x as f64 - a.x as f64);
        let py = s(mid.y as f64 - a.y as f64);
        let cross = s(r(py * dx) - r(px * dy));
        if cross < 0.0 {
            return false;
        }
        let len2 = x87.f32(r(dy * dy) + r(dx * dx));
        let dot = s(r(px * dx) + r(py * dy));
        let mut t = s(r(dot * fastmath::recip(len2) as f64));
        if !(t >= 0.0) {
            t = 0.0;
        }
        if t > 1.0 {
            t = 1.0;
        }
        let qy = s(r(py - s(r(dy * t))));
        let qx = s(r(px - s(r(dx * t))));
        let d2 = s(r(r(qx * qx) + r(qy * qy)));
        if d2 > 18496.0 {
            return false;
        }
        acc = s(r(s(d2.sqrt()) + acc));
        if !(acc <= 300.0) {
            return false;
        }
        if mid.flags & flag::NEAR_SEED != 0 {
            let seed_on_line = rect
                .cells_on_line([a.x, a.y], [v[cand].x, v[cand].y])
                .into_iter()
                .any(|(c, r)| {
                    let i = 2 * (r * grid.w + c);
                    grid.tris[i].flags & flag::SEED != 0 || grid.tris[i + 1].flags & flag::SEED != 0
                });
            if seed_on_line {
                return false;
            }
        }
    }
    true
}

/// The grid for `GridIterator` line walks.
struct LineRect {
    x0: f32,
    y_top: f32,
    w: usize,
    h: usize,
    x87: X87,
}

impl LineRect {
    /// Cells visited by `GridIterator_init_line` / `_advance` from `a` to
    /// `b`, reproducing the client's float arithmetic: the start cell, then
    /// one cell per crossed cell edge, trying the edge into the next row
    /// before the edge into the next column.
    fn cells_on_line(&self, a: [f32; 2], b: [f32; 2]) -> Vec<(usize, usize)> {
        let x87 = self.x87;
        let r = |v: f64| x87.r(v);
        let s = |v: f64| x87.f32(v) as f64;
        let (cw, ch) = (XY, XY);
        let x0 = self.x0 as f64;
        let y_top = self.y_top as f64;
        let x1 = s(r(self.w as f64 * cw) + x0);
        let y_bottom = s(y_top - r(self.h as f64 * ch));

        let dx = s(b[0] as f64 - a[0] as f64);
        let dy = s(b[1] as f64 - a[1] as f64);
        let (mut rem, dir) = if dx == 0.0 && dy == 0.0 {
            (0.0, [0.0, 0.0])
        } else {
            let len = s(r(r(dx * dx) + r(dy * dy)).sqrt());
            (len, [s(dx / len), s(dy / len)])
        };
        let (mut px, mut py) = (a[0] as f64, a[1] as f64);

        // Line/edge intersection: the edge parameter must be in [0, 1] and
        // the distance along the line in [0, rem].
        let clip = |px: &mut f64, py: &mut f64, rem: &mut f64, e: [f64; 4]| -> bool {
            let [ex, ey, fx, fy] = e;
            let den = s(r(r(fx * dir[1]) - r(fy * dir[0])));
            if den == 0.0 {
                return false;
            }
            let (qx, qy) = (r(ex - *px), r(ey - *py));
            let u = s(r(r(dir[0] * qy) - r(dir[1] * qx)) / den);
            let t = s(r(r(fx * qy) - r(fy * qx)) / den);
            if u < 0.0 || !(u <= 1.0 && 0.0 <= t && t <= *rem) {
                return false;
            }
            *px = s(r(dir[0] * t) + *px);
            *py = s(r(dir[1] * t) + *py);
            *rem = s(*rem - t);
            true
        };

        let inside = x0 <= px && y_bottom <= py && px < x1 && py < y_top;
        if !inside {
            if dir == [0.0, 0.0] {
                return Vec::new();
            }
            // Clip the start point onto the grid rectangle.
            let edges = [
                [x0, y_top, x1 - x0, 0.0],
                [x1, y_bottom, -(x1 - x0), 0.0],
                [x1, y_top, 0.0, -(y_top - y_bottom)],
                [x0, y_bottom, 0.0, y_top - y_bottom],
            ];
            let mut best = rem;
            let mut hit = false;
            for [ex, ey, fx, fy] in edges {
                let den = s(r(r(fx * dir[1]) - r(fy * dir[0])));
                if den == 0.0 {
                    continue;
                }
                let (qx, qy) = (r(ex - px), r(ey - py));
                let u = s(r(r(dir[0] * qy) - r(dir[1] * qx)) / den);
                let t = s(r(r(fx * qy) - r(fy * qx)) / den);
                if (0.0..=1.0).contains(&u) && 0.0 <= t && t <= best {
                    best = t;
                    hit = true;
                }
            }
            if !hit || rem < best {
                return Vec::new();
            }
            rem = s(rem - best);
            px = s(r(dir[0] * best) + px);
            py = s(best * dir[1] + py);
        }
        px = px.max(x0).min(x1);
        py = py.min(y_top).max(y_bottom);
        let mut col = (s(r(px - x0) / cw).floor() as usize).min(self.w - 1);
        let mut row = (s(r(y_top - py) / ch).floor() as usize).min(self.h - 1);
        let xl = s(r(col as f64 * cw) + x0);
        let yt = s(y_top - r(row as f64 * ch));
        let yb = s(yt - ch);
        let xr = s(cw + xl);
        // Top (rightwards), bottom (leftwards), left (up), right (down).
        let mut edges = [
            [xl, yt, s(xr - xl), 0.0],
            [xr, yb, -s(xr - xl), 0.0],
            [xl, yb, 0.0, s(yt - yb)],
            [xr, yt, 0.0, -s(yt - yb)],
        ];

        let mut cells = vec![(col, row)];
        loop {
            let mut moved = false;
            if dir[1] > 0.0 && clip(&mut px, &mut py, &mut rem, edges[0]) {
                let Some(nr) = row.checked_sub(1) else { break };
                row = nr;
                edges.iter_mut().for_each(|e| e[1] = s(e[1] + ch));
                moved = true;
            } else if dir[1] < 0.0 && clip(&mut px, &mut py, &mut rem, edges[1]) {
                row += 1;
                edges.iter_mut().for_each(|e| e[1] = s(e[1] - ch));
                moved = true;
            }
            if !moved {
                if dir[0] > 0.0 {
                    if !clip(&mut px, &mut py, &mut rem, edges[3]) {
                        break;
                    }
                    col += 1;
                    edges.iter_mut().for_each(|e| e[0] = s(e[0] + cw));
                } else if dir[0] < 0.0 {
                    if !clip(&mut px, &mut py, &mut rem, edges[2]) {
                        break;
                    }
                    let Some(nc) = col.checked_sub(1) else { break };
                    col = nc;
                    edges.iter_mut().for_each(|e| e[0] = s(e[0] - cw));
                } else {
                    break;
                }
            }
            if col >= self.w || row >= self.h {
                break;
            }
            cells.push((col, row));
        }
        cells
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::mapfile::Ffna;
    use crate::mapfile::navmesh::parse_planes;
    use crate::mapfile::params::MapParams;
    use crate::mapfile::path::{PathBloated, PathStrip};
    use crate::mapfile::props::PropsCollision;
    use crate::mapfile::terrain::TerrainStrip;
    use crate::mapfile::testdata::MapPair;

    /// The traced outline must reproduce the client's exactly: every vertex
    /// is among the client's plane-0 vertices, and the client's vertex list
    /// (in insertion order, i.e. shuffled segment order) equals ours once
    /// the vertices the trapezoid builder adds later (segment intersections
    /// and prop outlines) are filtered out.
    #[test]
    fn traced_outline_matches_client() {
        for pair in MapPair::all() {
            let (Some(strip), Some(bloated)) = (pair.strip(), pair.bloated()) else { continue };
            let strip = Ffna::parse(&strip).unwrap();
            let bloated = Ffna::parse(&bloated).unwrap();
            let terrain = TerrainStrip::parse(strip.chunk(0x1000_0002).unwrap()).unwrap();
            let path = PathStrip::parse(strip.chunk(0x1000_0008).unwrap()).unwrap();
            let params = MapParams::parse(strip.chunk(0x1000_000C).unwrap()).unwrap();
            let [dx, dy] = terrain.header.dims;
            let heights: Vec<f32> =
                (0..dy).flat_map(|y| (0..dx).map(move |x| (x, y))).map(|(x, y)| terrain.height(x, y)).collect();
            // Portal points from the client's own props bloat until ours exists.
            let props = PropsCollision::parse_bloated(bloated.chunk(0x2000_0004).unwrap()).unwrap();
            let client = PathBloated::parse(bloated.chunk(0x2000_0008).unwrap()).unwrap();
            let planes = parse_planes(client.planes).unwrap();

            let out = trace(&TracerInput {
                map_type: params.map_type(),
                water: params.has_water(),
                dims: terrain.header.dims,
                heights: &heights,
                min_x: params.min_x,
                max_y: params.max_y,
                start_points: &path.start_points,
                portal_points: &props.portal_points,
                x87: X87::Double,
            });

            let key = |x: f64, y: f64| ((x as f32).to_bits(), (y as f32).to_bits());
            let mut seen = HashSet::new();
            let mut ours = Vec::new();
            for seg in &out.segments {
                for p in [seg.p0, seg.p1] {
                    if seen.insert(key(p[0], p[1])) {
                        ours.push(key(p[0], p[1]));
                    }
                }
            }
            let client: Vec<(u32, u32)> = planes[0]
                .vectors
                .iter()
                .map(|v| key(v[0] as f64, v[1] as f64))
                .filter(|k| seen.contains(k))
                .collect();
            eprintln!(
                "{pair:?}: {} polygons, {} segments, {}/{} vertices found in client order",
                out.polygons.len(),
                out.segments.len(),
                client.len(),
                ours.len()
            );
            assert_eq!(client.len(), ours.len(), "{pair:?}: traced vertices missing from the client's");
            assert!(client == ours, "{pair:?}: vertex order differs from the client's");
        }
    }
}
