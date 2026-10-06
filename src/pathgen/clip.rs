//! Segment clipping (`PathBsp_ClipSegment`) and vertex registration
//! (`PathMapBuilder_InitSegments`, `PathMapNode_init_with_hash`).
//!
//! The client keeps the inserted segments in a BSP. Clipping a new segment
//! yields the parameters where it meets earlier ones and the ranges it
//! shares with collinear ones. The visiting order of the BSP only affects
//! internal allocation, so this uses a grid index over the inserted
//! segments instead; the arithmetic is the client's (x87 at 53 bits, i.e.
//! plain f64).

use std::collections::HashMap;

use super::assemble::Segment;

/// A split parameter along the new segment. `new_vertex` is false where
/// the point already exists (an endpoint of an earlier segment).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    pub t: f64,
    pub new_vertex: bool,
}

const CELL: f64 = 1024.0;

#[derive(Default)]
pub struct Clipper {
    nodes: Vec<Segment>,
    grid: HashMap<(i64, i64), Vec<usize>>,
}

fn bounds(s: &Segment) -> ([f64; 2], [f64; 2]) {
    (
        [s.p0[0].min(s.p1[0]), s.p0[1].min(s.p1[1])],
        [s.p0[0].max(s.p1[0]), s.p0[1].max(s.p1[1])],
    )
}

fn cells(lo: [f64; 2], hi: [f64; 2]) -> impl Iterator<Item = (i64, i64)> {
    let c = |v: f64| (v / CELL).floor() as i64;
    let (x0, x1, y0, y1) = (c(lo[0]), c(hi[0]), c(lo[1]), c(hi[1]));
    (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (x, y)))
}

/// `PathBsp_InsertRange`: keep sorted, merged covered ranges. Merging
/// reproduces the client, which extends by later ranges against the new
/// range's original end.
fn insert_range(ranges: &mut Vec<[f64; 2]>, new: [f64; 2]) {
    for i in 0..ranges.len() {
        let r = ranges[i];
        if new[0] <= r[1] {
            if r[0] <= new[1] {
                let mut lo = new[0];
                let mut hi = new[1];
                if hi <= r[1] {
                    hi = r[1];
                }
                if r[0] <= lo {
                    lo = r[0];
                }
                while i + 1 < ranges.len() && ranges[i + 1][0] <= new[1] {
                    hi = ranges[i + 1][1];
                    if hi <= new[1] {
                        hi = new[1];
                    }
                    ranges.remove(i + 1);
                }
                ranges[i] = [lo, hi];
                return;
            }
            ranges.insert(i, new);
            return;
        }
    }
    ranges.push(new);
}

impl Clipper {
    fn add_node(&mut self, s: Segment) {
        let index = self.nodes.len();
        let (lo, hi) = bounds(&s);
        for cell in cells(lo, hi) {
            self.grid.entry(cell).or_default().push(index);
        }
        self.nodes.push(s);
    }

    /// Clip `seg` against the inserted segments and insert it. Returns the
    /// sorted split parameters (empty if the segment is fully covered) and
    /// the inserted part of the segment.
    pub fn clip(&mut self, seg: &Segment) -> (Vec<Split>, Segment) {
        if self.nodes.is_empty() {
            self.add_node(*seg);
            return (vec![Split { t: 0.0, new_vertex: true }, Split { t: 1.0, new_vertex: true }], *seg);
        }
        let mut params = vec![Split { t: 0.0, new_vertex: true }, Split { t: 1.0, new_vertex: true }];
        let mut ranges: Vec<[f64; 2]> = Vec::new();

        let (lo, hi) = bounds(seg);
        let mut candidates: Vec<usize> = cells(lo, hi)
            .filter_map(|c| self.grid.get(&c))
            .flatten()
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        let [sx, sy] = seg.p0;
        let [ex, ey] = seg.p1;
        let [vx, vy] = seg.vector;
        for &i in &candidates {
            let n = &self.nodes[i];
            let (nlo, nhi) = bounds(n);
            if nhi[0] < lo[0] || nlo[0] > hi[0] || nhi[1] < lo[1] || nlo[1] > hi[1] {
                continue;
            }
            let [nx, ny] = n.p0;
            let [nx1, ny1] = n.p1;
            let [nvx, nvy] = n.vector;
            let den = vx * nvy - vy * nvx;
            if den == 0.0 {
                if (ny1 - sy) * vx == vy * (nx1 - sx) {
                    if (nx == sx && ny == sy) || (nx1 == sx && ny1 == sy) {
                        params[0].new_vertex = false;
                    }
                    if (ex == nx && ey == ny) || (nx1 == ex && ny1 == ey) {
                        params[1].new_vertex = false;
                    }
                    let (a, b) = if vx.abs() <= vy.abs() {
                        ((ny - sy) / vy, (ny1 - sy) / vy)
                    } else {
                        ((nx - sx) / vx, (nx1 - sx) / vx)
                    };
                    let (lo_t, hi_t) = if b < a { (b, a) } else { (a, b) };
                    if 0.0 < lo_t && lo_t < 1.0 {
                        params.push(Split { t: lo_t, new_vertex: false });
                    }
                    if 0.0 < hi_t {
                        if hi_t < 1.0 {
                            params.push(Split { t: hi_t, new_vertex: false });
                        }
                        if lo_t < 1.0 {
                            insert_range(&mut ranges, [lo_t, hi_t]);
                        }
                    }
                }
                continue;
            }
            let t = (nvx * (sy - ny) - nvy * (sx - nx)) / den;
            let u = (vx * (sy - ny) - vy * (sx - nx)) / den;
            if t == 0.0 {
                if u == 0.0 || u == 1.0 {
                    params[0].new_vertex = false;
                }
            } else if t == 1.0 {
                if u == 0.0 || u == 1.0 {
                    params[1].new_vertex = false;
                }
            } else if 0.0 < t && t < 1.0 {
                if u == 0.0 || u == 1.0 {
                    params.push(Split { t, new_vertex: false });
                } else if 0.0 < u && u < 1.0 {
                    params.push(Split { t, new_vertex: true });
                }
            }
        }

        // Fully covered by collinear segments: nothing to insert.
        if let Some(r) = ranges.iter().find(|r| r[1] >= 0.0)
            && r[1] >= 1.0
            && r[0] <= 0.0
        {
            return (Vec::new(), *seg);
        }

        params.sort_by(|a, b| a.t.total_cmp(&b.t));
        let mut out: Vec<Split> = Vec::new();
        for p in params {
            let covered = ranges.iter().find(|r| r[1] > p.t).is_some_and(|r| p.t > r[0]);
            if covered {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.t == p.t => last.new_vertex &= p.new_vertex,
                _ => out.push(p),
            }
        }

        let (t0, t1) = (out[0].t, out[out.len() - 1].t);
        let p0 = [vx * t0 + sx, vy * t0 + sy];
        let p1 = [vx * t1 + sx, vy * t1 + sy];
        let clipped = Segment { p0, p1, vector: [p1[0] - p0[0], p1[1] - p0[1]], ..*seg };
        self.add_node(clipped);
        (out, clipped)
    }
}

/// `PathMapNode_init_with_hash`: vertices are truncated to 1/100 unit.
pub fn quantize(v: f64) -> f64 {
    ((v / 0.01) as i32) as f64 * 0.01
}

/// The vertex list of one plane, in the client's insertion order, from
/// clipping its segments in order.
pub fn plane_vertices(segments: &[Segment]) -> Vec<[f64; 2]> {
    let mut clipper = Clipper::default();
    let mut seen = std::collections::HashSet::new();
    let mut vertices = Vec::new();
    for seg in segments {
        for split in clipper.clip(seg).0 {
            if !split.new_vertex {
                continue;
            }
            let x = quantize(split.t * seg.vector[0] + seg.p0[0]);
            let y = quantize(split.t * seg.vector[1] + seg.p0[1]);
            if seen.insert((x.to_bits(), y.to_bits())) {
                vertices.push([x, y]);
            }
        }
    }
    vertices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathgen::assemble::assemble;
    use crate::pathgen::testing::Fixture;

    #[test]
    fn plane_vertices_match_client() {
        for f in Fixture::all() {
            let planes = assemble(&f.traced.segments, &f.start_points, &f.props.points);
            assert_eq!(planes.len(), f.client_planes.len(), "{:?}: plane count", f.pair);
            for (i, (plane, client)) in planes.iter().zip(&f.client_planes).enumerate() {
                let ours: Vec<[u32; 2]> =
                    plane_vertices(&plane.segments).iter().map(|v| [(v[0] as f32).to_bits(), (v[1] as f32).to_bits()]).collect();
                let theirs: Vec<[u32; 2]> = client.vectors.iter().map(|v| [v[0].to_bits(), v[1].to_bits()]).collect();
                let prefix = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
                if ours != theirs {
                    let f32s = |v: &[[u32; 2]]| v.iter().map(|p| (f32::from_bits(p[0]), f32::from_bits(p[1]))).collect::<Vec<_>>();
                    eprintln!(
                        "{:?} plane {i}: {} vs {} vertices, first {prefix} equal\n   ours   {:?}\n   client {:?}",
                        f.pair,
                        ours.len(),
                        theirs.len(),
                        f32s(&ours[prefix..(prefix + 3).min(ours.len())]),
                        f32s(&theirs[prefix..(prefix + 3).min(theirs.len())])
                    );
                }
                assert!(ours == theirs, "{:?} plane {i}: vertex list differs", f.pair);
            }
            eprintln!("{:?}: {} planes, all vertex lists equal", f.pair, planes.len());
        }
    }
}
