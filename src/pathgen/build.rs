//! The per-plane trapezoidal map builder: `PathMap_Build` and
//! `PathMapBuilder_*` / `PathBuild_*` / `PathTree_*` from the client's
//! `Engine\Map\Path\PathBuild.cpp`.
//!
//! Every segment is clipped against the earlier ones, and each piece is
//! threaded through the trapezoidal map (splitting trapezoids at its
//! endpoints and along it, then merging), maintaining the point location
//! DAG. Afterwards flat trapezoids are removed, everything unreachable from
//! the plane's start points is pruned, portals are linked, and the result is
//! numbered the way the client serializes it.
//!
//! Pointers become arena indices. The trapezoid arena's order is the
//! client's creation order, which is also its export order.

// The predicates keep the client's comparisons, which differ from their
// negations for the infinite bounds of the outer trapezoids.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use std::collections::HashMap;

use crate::mapfile::navmesh::{self, NodeRef, PathPlane};

use super::assemble::{PlaneInput, PortalInput, Segment};
use super::clip::{Clipper, quantize};

const EPS: f64 = 0.01;

type T = usize;
type N = usize;
type S = usize;

/// Segment geometry as the builder sees it (the 56-byte segment record).
#[derive(Debug, Clone, Copy)]
struct Seg {
    p0: [f64; 2],
    p1: [f64; 2],
    v: [f64; 2],
    portal: u16,
}

impl From<&Segment> for Seg {
    fn from(s: &Segment) -> Self {
        Seg { p0: s.p0, p1: s.p1, v: s.vector, portal: s.portal }
    }
}

#[derive(Debug, Clone)]
struct Trap {
    above: [Option<T>; 2],
    below: [Option<T>; 2],
    left: Option<S>,
    right: Option<S>,
    portal: [u16; 2],
    top_x: [f64; 2],
    top: [f64; 2],
    bot_x: [f64; 2],
    bottom: [f64; 2],
    node: N,
    alive: bool,
    index: u32,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    X { seg: Seg, left: Option<N>, right: Option<N> },
    Y { p: [f64; 2], above: Option<N>, below: Option<N> },
    Sink(T),
}

const REACHED: u8 = 1;
const DEAD: u8 = 2;
const SEEN: u8 = 4;
const WRITTEN: u8 = 8;
const DELETED: u8 = 0x10;

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    flags: u8,
    index: u32,
}

/// A portal of the plane being built (24-byte record).
#[derive(Debug, Clone, Default)]
struct BPortal {
    neighbor_plane: u16,
    id: u16,
    flags: u8,
    /// Trapezoids on each side, most recently added first; replaced by
    /// `(count, start)` into `portal_traps` once sorted.
    lists: [Vec<T>; 2],
    ranges: [(u32, u32); 2],
}

/// A portal's extent recorded for the neighbour plane
/// (`PathBuild_SortAndCreatePortals`).
#[derive(Debug, Clone, Copy)]
pub struct PortalRecord {
    top: [f64; 2],
    bottom: [f64; 2],
}

/// Shared between planes: portal extents keyed `portal | plane << 16`.
pub type PortalRecords = HashMap<u32, PortalRecord>;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= EPS
}

/// `PathBuild_InterpolateSegmentX`.
fn interp(s: &Seg, y: f64) -> f64 {
    assert!(s.v[1] != 0.0, "segment.vector.y");
    if y == s.p0[1] {
        return s.p0[0];
    }
    if s.p1[1] == y {
        return s.p1[0];
    }
    ((y - s.p0[1]) / s.v[1]) * s.v[0] + s.p0[0]
}

struct Builder<'a> {
    plane: u16,
    subs: Vec<Seg>,
    traps: Vec<Trap>,
    nodes: Vec<Node>,
    root: Option<N>,
    vertices: Vec<[f64; 2]>,
    vertex_index: HashMap<(u64, u64), u32>,
    portals: Vec<BPortal>,
    portal_traps: Vec<T>,
    failed: bool,
    records: &'a mut PortalRecords,
}

impl Builder<'_> {
    // ----- arena helpers -------------------------------------------------

    fn t(&self, t: T) -> &Trap {
        &self.traps[t]
    }

    fn tm(&mut self, t: T) -> &mut Trap {
        &mut self.traps[t]
    }

    fn new_trap(&mut self) -> T {
        self.traps.push(Trap {
            above: [None; 2],
            below: [None; 2],
            left: None,
            right: None,
            portal: [0xFFFF; 2],
            top_x: [0.0; 2],
            top: [0.0; 2],
            bot_x: [0.0; 2],
            bottom: [0.0; 2],
            node: usize::MAX,
            alive: true,
            index: 0,
        });
        self.traps.len() - 1
    }

    fn new_sink(&mut self, t: T) -> N {
        self.nodes.push(Node { kind: Kind::Sink(t), flags: 0, index: 0 });
        let n = self.nodes.len() - 1;
        self.tm(t).node = n;
        n
    }

    fn add_vertex(&mut self, p: [f64; 2]) {
        let q = [quantize(p[0]), quantize(p[1])];
        let key = (q[0].to_bits(), q[1].to_bits());
        if !self.vertex_index.contains_key(&key) {
            self.vertex_index.insert(key, self.vertices.len() as u32);
            self.vertices.push(q);
        }
    }

    fn vertex_of(&self, p: [f64; 2]) -> u32 {
        let key = (quantize(p[0]).to_bits(), quantize(p[1]).to_bits());
        *self.vertex_index.get(&key).expect("vertex")
    }

    /// Replace `old` by `new` in `n`'s below links, then compact them.
    fn relink_below(&mut self, n: T, old: T, new: Option<T>) {
        let tr = self.tm(n);
        for b in &mut tr.below {
            if *b == Some(old) {
                *b = new;
            }
        }
        if tr.below[0].is_none() && tr.below[1].is_some() {
            tr.below = [tr.below[1], None];
        } else if tr.below[0] == tr.below[1] {
            tr.below[1] = None;
        }
    }

    fn relink_above(&mut self, n: T, old: T, new: Option<T>) {
        let tr = self.tm(n);
        for a in &mut tr.above {
            if *a == Some(old) {
                *a = new;
            }
        }
        if tr.above[0].is_none() && tr.above[1].is_some() {
            tr.above = [tr.above[1], None];
        } else if tr.above[0] == tr.above[1] {
            tr.above[1] = None;
        }
    }

    // ----- geometry predicates ------------------------------------------

    /// `PathBuild_InitTrapezoidEdges`.
    fn init_edges(&mut self, t: T, left: Option<S>, right: Option<S>, top: [f64; 2], bottom: [f64; 2]) {
        let side = |s: Option<S>, inf: f64| match s {
            None => (inf, inf),
            Some(s) => {
                let s = &self.subs[s];
                if (s.p1[1] - s.p0[1]).abs() > EPS {
                    (interp(s, top[1]), interp(s, bottom[1]))
                } else {
                    (top[0], bottom[0])
                }
            }
        };
        let (tl, bl) = side(left, f64::NEG_INFINITY);
        let (tr, br) = side(right, f64::INFINITY);
        let trap = self.tm(t);
        trap.left = left;
        trap.right = right;
        trap.top_x = [tl, tr];
        trap.bot_x = [bl, br];
        trap.top = top;
        trap.bottom = bottom;
    }

    /// `PathBuild_SegmentIntersectsTrapezoid`: does the segment cross the
    /// trapezoid's top edge?
    fn crosses_top(&self, s: &Seg, t: T) -> bool {
        let t = self.t(t);
        if (s.p1[1] - s.p0[1]).abs() <= EPS {
            return false;
        }
        let ty = t.top[1];
        if (s.p1[1] - ty).abs() > EPS {
            if s.p0[1] < ty {
                return false;
            }
        } else if t.top[0] <= s.p1[0] {
            return false;
        }
        if (s.p0[1] - ty).abs() > EPS {
            if ty < s.p1[1] {
                return false;
            }
        } else if !(t.top[0] < s.p1[0]) {
            return false;
        }
        let x = interp(s, ty);
        if t.top_x[1] < x && !approx(x, t.top_x[1]) {
            return false;
        }
        if !(t.top_x[0] <= x) {
            return approx(x, t.top_x[0]);
        }
        true
    }

    /// `PathBuild_CheckSegmentIntersection`: does the segment cross the
    /// trapezoid's bottom edge?
    fn crosses_bottom(&self, s: &Seg, t: T) -> bool {
        let t = self.t(t);
        if (s.p1[1] - s.p0[1]).abs() <= EPS {
            return false;
        }
        let by = t.bottom[1];
        let upper_ok = if (s.p0[1] - by).abs() <= EPS { t.bottom[0] < s.p0[0] } else { by <= s.p0[1] };
        if !upper_ok {
            return false;
        }
        let lower_ok = if (s.p1[1] - by).abs() <= EPS { !(t.bottom[0] <= s.p1[0]) } else { !(by < s.p1[1]) };
        if !lower_ok {
            return false;
        }
        let x = interp(s, by);
        if t.bot_x[1] < x && !approx(x, t.bot_x[1]) {
            return false;
        }
        if !(t.bot_x[0] <= x) {
            return approx(x, t.bot_x[0]);
        }
        true
    }

    /// `PathBuild_PointInTrapezoid`.
    fn contains(&self, t: T, p: [f64; 2]) -> bool {
        let t = self.t(t);
        if !(p[1] < t.top[1]) {
            return false;
        }
        let above_bottom =
            if (p[1] - t.bottom[1]).abs() > EPS { t.bottom[1] <= p[1] } else { t.bottom[0] <= p[0] };
        if !above_bottom {
            return false;
        }
        if let Some(l) = t.left {
            let l = &self.subs[l];
            if !(0.0 <= (p[1] - l.p0[1]) * l.v[0] - (p[0] - l.p0[0]) * l.v[1]) {
                return false;
            }
        }
        if let Some(r) = t.right {
            let r = &self.subs[r];
            if !((p[1] - r.p0[1]) * r.v[0] - (p[0] - r.p0[0]) * r.v[1] <= 0.0) {
                return false;
            }
        }
        true
    }

    /// `PathTree_QueryByCoords`.
    fn query(&self, q: [f64; 2]) -> Option<T> {
        let mut n = self.root?;
        loop {
            match self.nodes[n].kind {
                Kind::Sink(t) => return Some(t),
                Kind::Y { p, above, below } => {
                    let go_below = q[1] < p[1] || (q[1] == p[1] && p[0] > q[0]);
                    n = if go_below { below? } else { above? };
                }
                Kind::X { seg, left, right } => {
                    let side = (q[1] - seg.p0[1]) * seg.v[0] - (q[0] - seg.p0[0]) * seg.v[1];
                    n = if 0.0 < side { right? } else { left? };
                }
            }
        }
    }

    // ----- structural operations ----------------------------------------

    /// `PathBuild_SplitTrapezoidVertical`: split `t` at point `p` into an
    /// upper and a lower trapezoid; its sink becomes a Y node.
    fn split_at(&mut self, p: [f64; 2], t: T) -> T {
        let u = self.new_trap();
        let l = self.new_trap();
        let (left, right, top, bottom) = {
            let o = self.t(t);
            (o.left, o.right, o.top, o.bottom)
        };
        self.init_edges(u, left, right, top, p);
        self.init_edges(l, left, right, p, bottom);
        let (above, below) = (self.t(t).above, self.t(t).below);
        self.tm(u).above = above;
        self.tm(u).below = [Some(l), None];
        self.tm(l).above = [Some(u), None];
        self.tm(l).below = below;
        for a in above.into_iter().flatten() {
            self.relink_below(a, t, Some(u));
        }
        for b in below.into_iter().flatten() {
            self.relink_above(b, t, Some(l));
        }
        let nu = self.new_sink(u);
        let nl = self.new_sink(l);
        let old = self.t(t).node;
        self.tm(t).alive = false;
        self.nodes[old].kind = Kind::Y { p, above: Some(nu), below: Some(nl) };
        u
    }

    fn insert(&mut self, input: &Segment, clipper: &mut Clipper) {
        let (splits, clipped) = clipper.clip(input);
        if splits.is_empty() {
            return;
        }
        let seg = Seg::from(&clipped);
        let points: Vec<[f64; 2]> = splits
            .iter()
            .map(|s| [s.t * input.vector[0] + input.p0[0], s.t * input.vector[1] + input.p0[1]])
            .collect();
        for (s, p) in splits.iter().zip(&points) {
            if s.new_vertex {
                self.add_vertex(*p);
            }
        }
        for w in points.windows(2) {
            let (p0, p1) = (w[0], w[1]);
            self.subs.push(Seg { p0, p1, v: [p1[0] - p0[0], p1[1] - p0[1]], portal: seg.portal });
            let sub = self.subs.len() - 1;
            self.insert_sub(sub, &seg);
        }
    }

    /// Thread one sub-segment through the map (`PathMapBuilder_InitSegments`
    /// after clipping).
    fn insert_sub(&mut self, sub: S, seg: &Seg) {
        let s = self.subs[sub];
        let q = [s.v[0] * 0.1 + s.p0[0], s.v[1] * 0.1 + s.p0[1]];
        let mut next = self.query(q).expect("query");
        let start = loop {
            let cur = next;
            let [a0, a1] = self.t(cur).above;
            let cand = if let (Some(a0), Some(a1)) = (a0, a1) {
                if self.crosses_bottom(&s, a0) {
                    a0
                } else if self.crosses_bottom(&s, a1) {
                    a1
                } else if self.crosses_top(&s, a0) {
                    a0
                } else if self.crosses_top(&s, a1) {
                    a1
                } else if self.contains(a0, s.p0) {
                    a0
                } else if self.contains(a1, s.p0) {
                    a1
                } else {
                    break cur;
                }
            } else {
                match a0 {
                    Some(a0)
                        if self.crosses_top(&s, a0) || self.crosses_bottom(&s, a0) || self.contains(a0, s.p0) =>
                    {
                        a0
                    }
                    _ => break cur,
                }
            };
            let b = self.t(cand).bottom;
            let stop = if (b[1] - s.p0[1]).abs() > EPS { s.p0[1] < b[1] } else { s.p0[0] <= b[0] };
            if stop {
                break cur;
            }
            next = cand;
        };
        if s.p0 != self.t(start).top {
            self.split_at(s.p0, start);
        }
        let mut bottom = self.find_segment_trap(&s);
        if s.p1 != self.t(bottom).bottom {
            bottom = self.split_at(s.p1, bottom);
        }
        let list = self.create_trapezoids(seg, &s, bottom);
        self.split_along(&list, sub);
        self.convert_to_leaves(&list, seg);
    }

    /// `PathTree_FindSegmentTrapezoid`: the lowest trapezoid along the
    /// sub-segment, walking down from just above its lower end.
    fn find_segment_trap(&self, s: &Seg) -> T {
        let q = [s.p1[0] - s.v[0] * 0.1, s.p1[1] - s.v[1] * 0.1];
        let mut next = self.query(q).expect("query");
        loop {
            let cur = next;
            let [b0, b1] = self.t(cur).below;
            let cand = if let (Some(b0), Some(b1)) = (b0, b1) {
                if self.crosses_top(s, b0) {
                    b0
                } else if self.crosses_top(s, b1) {
                    b1
                } else if self.crosses_bottom(s, b0) {
                    b0
                } else if self.crosses_bottom(s, b1) {
                    b1
                } else if self.contains(b0, s.p0) {
                    b0
                } else if self.contains(b1, s.p0) {
                    b1
                } else {
                    return cur;
                }
            } else {
                match b0 {
                    Some(b0)
                        if self.crosses_top(s, b0) || self.crosses_bottom(s, b0) || self.contains(b0, s.p0) =>
                    {
                        b0
                    }
                    _ => return cur,
                }
            };
            let top = self.t(cand).top;
            let stop = if (top[1] - s.p1[1]).abs() > EPS { top[1] < s.p1[1] } else { top[0] <= s.p1[0] };
            if stop {
                return cur;
            }
            next = cand;
        }
    }

    /// `PathBuild_CreateTrapezoids`: the trapezoids the sub-segment crosses,
    /// bottom to top, each with a new left and right trapezoid.
    fn create_trapezoids(&mut self, seg: &Seg, s: &Seg, start: T) -> Vec<(T, T, T)> {
        let mut list = Vec::new();
        let mut t = start;
        loop {
            let l = self.new_trap();
            let r = self.new_trap();
            list.push((t, l, r));
            let top = self.t(t).top;
            if (s.p0[1] - top[1]).abs() <= EPS && s.p0[0] <= top[0] {
                return list;
            }
            let [a0, a1] = self.t(t).above;
            let a0 = a0.expect("trapezoid->above[0]");
            match if self.crosses_bottom(seg, a0) { Some(a0) } else { a1 } {
                Some(n) => t = n,
                None => return list,
            }
        }
    }

    /// `PathBuild_SplitTrapezoidsHorizontal`: fill in the left and right
    /// trapezoids and their links.
    fn split_along(&mut self, list: &[(T, T, T)], sub: S) {
        let n = list.len();
        for i in 0..n {
            let (orig, l, r) = list[i];
            let (oleft, oright, top, bottom) = {
                let o = self.t(orig);
                (o.left, o.right, o.top, o.bottom)
            };
            self.init_edges(l, oleft, Some(sub), top, bottom);
            self.init_edges(r, Some(sub), oright, top, bottom);
            self.new_sink(l);
            self.new_sink(r);
            let (oa, ob) = (self.t(orig).above, self.t(orig).below);

            if i == n - 1 {
                self.distribute_top(orig, l, r);
            } else {
                let (next, nl, nr) = list[i + 1];
                if oa[0] == Some(next) {
                    self.tm(l).above = [Some(nl), None];
                    self.tm(r).above = [Some(nr), oa[1]];
                    if let Some(a1) = oa[1] {
                        self.tm(a1).below[0] = Some(r);
                    }
                } else {
                    assert_eq!(oa[1], Some(next), "orig->above[1] == next.orig");
                    self.tm(l).above = [oa[0], Some(nl)];
                    self.tm(r).above = [Some(nr), None];
                    if let Some(a0) = self.t(l).above[0] {
                        self.tm(a0).below[0] = Some(l);
                    }
                }
            }

            if i == 0 {
                self.distribute_bottom(orig, l, r);
            } else {
                let (prev, pl, pr) = list[i - 1];
                if ob[0] == Some(prev) {
                    self.tm(l).below = [Some(pl), None];
                    self.tm(r).below = [Some(pr), ob[1]];
                    if let Some(b1) = ob[1] {
                        self.tm(b1).above[0] = Some(r);
                    }
                } else {
                    assert_eq!(ob[1], Some(prev), "orig->below[1] == prev.orig");
                    self.tm(l).below = [ob[0], Some(pl)];
                    self.tm(r).below = [Some(pr), None];
                    if let Some(b0) = ob[0] {
                        self.tm(b0).above[0] = Some(l);
                    }
                }
            }
        }
    }

    /// `PathBuild_DistributeAboveLinks`: below links of the bottom pair.
    fn distribute_bottom(&mut self, orig: T, l: T, r: T) {
        let ldeg = (self.t(l).bot_x[0] - self.t(l).bot_x[1]).abs() <= EPS;
        let rdeg = (self.t(r).bot_x[0] - self.t(r).bot_x[1]).abs() <= EPS;
        let ob = self.t(orig).below;
        match (ldeg, rdeg) {
            (true, true) => {
                self.tm(l).below = [None; 2];
                self.tm(r).below = [None; 2];
            }
            (false, false) => {
                self.tm(l).below = [ob[0], None];
                self.tm(r).below = [ob[1].or(ob[0]), None];
            }
            (true, false) => {
                self.tm(r).below = ob;
                self.tm(l).below = [None; 2];
            }
            (false, true) => {
                self.tm(l).below = ob;
                self.tm(r).below = [None; 2];
            }
        }
        let r_or_none = if rdeg { None } else { Some(r) };
        if ob[1].is_none() {
            let Some(b) = ob[0] else { return };
            assert!(self.t(b).above[0].is_some(), "below->above[0]");
            let a1 = self.t(b).above[1];
            match a1 {
                None => {
                    if ldeg {
                        self.tm(b).above = [r_or_none, None];
                        return;
                    }
                    self.tm(b).above[0] = Some(l);
                }
                Some(a1) => {
                    if self.t(b).above[0] == Some(orig) {
                        if ldeg {
                            self.tm(b).above = [Some(a1), None];
                        } else {
                            self.tm(b).above[0] = Some(l);
                        }
                        return;
                    }
                    assert_eq!(a1, orig, "below->above[1] == orig");
                }
            }
            self.tm(b).above[1] = r_or_none;
        } else {
            for (t, links) in [(l, self.t(l).below), (r, self.t(r).below)] {
                for b in links.into_iter().flatten() {
                    self.tm(b).above[0] = Some(t);
                }
            }
        }
    }

    /// `PathBuild_DistributeBelowLinks`: above links of the top pair.
    fn distribute_top(&mut self, orig: T, l: T, r: T) {
        let ldeg = (self.t(l).top_x[0] - self.t(l).top_x[1]).abs() <= EPS;
        let rdeg = (self.t(r).top_x[0] - self.t(r).top_x[1]).abs() <= EPS;
        let oa = self.t(orig).above;
        match (ldeg, rdeg) {
            (true, true) => {
                self.tm(l).above = [None; 2];
                self.tm(r).above = [None; 2];
            }
            (true, false) => {
                self.tm(l).above = [None; 2];
                self.tm(r).above = oa;
            }
            (false, true) => {
                self.tm(l).above = oa;
                self.tm(r).above = [None; 2];
            }
            (false, false) => {
                self.tm(l).above = [oa[0], None];
                self.tm(r).above = [oa[1].or(oa[0]), None];
            }
        }
        let r_or_none = if rdeg { None } else { Some(r) };
        if oa[1].is_none() {
            let Some(a) = oa[0] else { return };
            assert!(self.t(a).below[0].is_some(), "above->below[0]");
            let b1 = self.t(a).below[1];
            match b1 {
                None => {
                    assert_eq!(self.t(a).below[0], Some(orig), "above->below[0] == orig");
                    if ldeg {
                        self.tm(a).below = [r_or_none, None];
                        return;
                    }
                    self.tm(a).below[0] = Some(l);
                }
                Some(b1) => {
                    if self.t(a).below[0] == Some(orig) {
                        if ldeg {
                            self.tm(a).below = [Some(b1), None];
                        } else {
                            self.tm(a).below[0] = Some(l);
                        }
                        return;
                    }
                    assert_eq!(b1, orig, "above->below[1] == orig");
                }
            }
            self.tm(a).below[1] = r_or_none;
        } else {
            for (t, links) in [(l, self.t(l).above), (r, self.t(r).above)] {
                for a in links.into_iter().flatten() {
                    self.tm(a).below[0] = Some(t);
                }
            }
        }
    }

    /// `PathBuild_ConvertTrapezoidsToLeaves`: the crossed trapezoids' sinks
    /// become X nodes; vertically adjacent pieces with the same sides merge.
    fn convert_to_leaves(&mut self, list: &[(T, T, T)], seg: &Seg) {
        let (_, mut l, mut r) = list[0];
        for (i, &(orig, _, _)) in list.iter().enumerate() {
            let node = self.t(orig).node;
            self.tm(orig).alive = false;
            self.nodes[node].kind =
                Kind::X { seg: *seg, left: Some(self.t(l).node), right: Some(self.t(r).node) };
            if i + 1 == list.len() {
                return;
            }
            let (_, mut nl, mut nr) = list[i + 1];
            if self.t(l).left == self.t(nl).left && self.t(l).right == self.t(nl).right {
                self.merge(l, nl);
                nl = l;
            }
            if self.t(r).left == self.t(nr).left && self.t(r).right == self.t(nr).right {
                self.merge(r, nr);
                nr = r;
            }
            (l, r) = (nl, nr);
        }
    }

    /// `PathBuild_MergeTrapezoids`: `upper` is absorbed into `lower`.
    fn merge(&mut self, lower: T, upper: T) {
        let u = self.t(upper).clone();
        {
            let t = self.tm(lower);
            t.above = u.above;
            t.top_x = u.top_x;
            t.top = u.top;
        }
        for a in u.above.into_iter().flatten() {
            self.relink_below(a, upper, Some(lower));
        }
        self.tm(upper).alive = false;
    }

    // ----- cleanup -------------------------------------------------------

    fn is_degenerate(&self, t: T) -> bool {
        let t = self.t(t);
        let portal_side = |s: Option<S>| s.is_some_and(|s| self.subs[s].portal != 0xFFFF);
        t.bottom[1] == t.top[1]
            && t.below[1].is_none()
            && t.above[1].is_none()
            && !portal_side(t.left)
            && !portal_side(t.right)
    }

    fn degenerate_sink(&self, n: Option<N>) -> Option<N> {
        let n = n?;
        match self.nodes[n].kind {
            Kind::Sink(t) if self.is_degenerate(t) => Some(n),
            _ => None,
        }
    }

    /// `PathBuild_RemoveDegenerateTrapezoids`: one pass; returns whether
    /// anything was removed.
    fn remove_degenerate(&mut self) -> bool {
        let Some(root) = self.root else { return false };
        let mut stack = vec![root];
        let mut processed = Vec::new();
        let mut found: Vec<N> = Vec::new();
        let mark = |b: &mut Builder, d: N, found: &mut Vec<N>| {
            if b.nodes[d].flags & SEEN == 0 {
                b.nodes[d].flags |= SEEN;
                found.push(d);
            }
        };
        while let Some(n) = stack.pop() {
            if self.nodes[n].flags & SEEN != 0 {
                continue;
            }
            match self.nodes[n].kind {
                Kind::X { left, right, .. } => {
                    if let Some(d) = self.degenerate_sink(left) {
                        mark(self, d, &mut found);
                        self.set_child(n, 0, None);
                    } else if let Some(c) = left {
                        stack.push(c);
                    }
                    if let Some(d) = self.degenerate_sink(right) {
                        mark(self, d, &mut found);
                        self.set_child(n, 1, None);
                    } else if let Some(c) = right {
                        stack.push(c);
                    }
                }
                Kind::Y { above, below, .. } => {
                    if let Some(d) = self.degenerate_sink(above) {
                        mark(self, d, &mut found);
                        let Kind::Sink(t) = self.nodes[d].kind else { unreachable!() };
                        let mut c = self.t(t).below[0];
                        while let Some(x) = c {
                            if !self.is_degenerate(x) {
                                break;
                            }
                            c = self.t(x).below[0];
                        }
                        let repl = c.map(|x| self.t(x).node);
                        self.set_child(n, 0, repl);
                    } else if let Some(c) = above {
                        stack.push(c);
                    }
                    if let Some(d) = self.degenerate_sink(below) {
                        mark(self, d, &mut found);
                        let Kind::Sink(t) = self.nodes[d].kind else { unreachable!() };
                        let mut c = self.t(t).above[0];
                        while let Some(x) = c {
                            if !self.is_degenerate(x) {
                                break;
                            }
                            c = self.t(x).above[0];
                        }
                        let repl = c.map(|x| self.t(x).node);
                        self.set_child(n, 1, repl);
                    } else if let Some(c) = below {
                        stack.push(c);
                    }
                }
                Kind::Sink(_) => continue,
            }
            processed.push(n);
            self.nodes[n].flags |= SEEN;
        }
        if found.is_empty() {
            return false;
        }
        for n in processed {
            self.nodes[n].flags &= !SEEN;
        }
        for d in found {
            if Some(d) == self.root {
                continue;
            }
            let Kind::Sink(t) = self.nodes[d].kind else { unreachable!() };
            let (a, b) = (self.t(t).above[0], self.t(t).below[0]);
            if let Some(a) = a {
                self.relink_below(a, t, b);
            }
            if let Some(b) = b {
                self.relink_above(b, t, a);
            }
            self.tm(t).alive = false;
        }
        true
    }

    fn set_child(&mut self, n: N, which: usize, child: Option<N>) {
        match &mut self.nodes[n].kind {
            Kind::X { left, right, .. } => *[left, right][which] = child,
            Kind::Y { above, below, .. } => *[above, below][which] = child,
            Kind::Sink(_) => unreachable!(),
        }
    }

    /// `PathTree_MarkVisitedBFS`: flag every trapezoid reachable from a start
    /// point through neighbour links.
    fn mark_reachable(&mut self, starts: &[[f64; 2]]) {
        for p in starts {
            let Some(t) = self.query(*p) else { continue };
            let mut stack = vec![t];
            while let Some(t) = stack.pop() {
                let n = self.t(t).node;
                if self.nodes[n].flags & REACHED != 0 {
                    continue;
                }
                self.nodes[n].flags |= REACHED;
                let tr = self.t(t);
                stack.extend(tr.above.into_iter().chain(tr.below).flatten());
            }
        }
    }

    /// `PathTree_ValidateRecursive`: flag subtrees without reachable
    /// trapezoids as dead.
    fn mark_dead(&mut self, n: Option<N>) -> bool {
        let Some(n) = n else { return true };
        if self.nodes[n].flags & DEAD != 0 {
            return true;
        }
        let dead = match self.nodes[n].kind {
            Kind::Sink(_) => {
                self.nodes[n].flags & REACHED == 0
            }
            Kind::X { left, right, .. } => {
                let l = self.mark_dead(left);
                l & self.mark_dead(right)
            }
            Kind::Y { above, below, .. } => {
                let a = self.mark_dead(above);
                let b = self.mark_dead(below);
                a && b
            }
        };
        if dead {
            self.nodes[n].flags |= DEAD;
        }
        dead
    }

    /// `PathBuild_ProcessNodeStack`: cut dead nodes out and free their
    /// trapezoids.
    fn prune(&mut self) {
        let Some(root) = self.root else { return };
        let mut stack = vec![root];
        let mut visited = vec![false; self.nodes.len()];
        while let Some(n) = stack.pop() {
            if self.nodes[n].flags & DELETED != 0 {
                continue;
            }
            if self.nodes[n].flags & DEAD != 0 {
                self.nodes[n].flags |= DELETED;
                if let Kind::Sink(t) = self.nodes[n].kind {
                    self.tm(t).alive = false;
                }
            } else if std::mem::replace(&mut visited[n], true) {
                continue;
            }
            let children = match self.nodes[n].kind {
                Kind::X { left, right, .. } => [left, right],
                Kind::Y { above, below, .. } => [above, below],
                Kind::Sink(_) => continue,
            };
            for (i, c) in children.into_iter().enumerate() {
                if let Some(c) = c {
                    stack.push(c);
                    if self.nodes[c].flags & DEAD != 0 {
                        self.set_child(n, i, None);
                    }
                }
            }
        }
    }

    // ----- portals --------------------------------------------------------

    fn live(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.traps.len()).filter(|&t| self.traps[t].alive)
    }

    /// `PathBuild_InsertEdgeEndpoints`: match a portal edge to the extent
    /// recorded by the neighbour plane, splitting `t` where needed.
    fn insert_edge_endpoints(&mut self, portal: u16, side: usize, t: T) -> T {
        let rec = &self.portals[portal as usize];
        let key = (rec.neighbor_plane as u32) << 16 | rec.id as u32;
        let Some(rec) = self.records.get(&key).copied() else { return t };
        let clear_side = |b: &mut Builder, t: T| {
            if side == 0 {
                b.tm(t).left = None;
            } else {
                b.tm(t).right = None;
            }
        };
        let top_y = rec.top[1].min(self.t(t).top[1]);
        let bottom_y = rec.bottom[1].max(self.t(t).bottom[1]);
        if top_y < bottom_y {
            clear_side(self, t);
            return t;
        }
        let mut t = t;
        let mut lower = t;
        if top_y != self.t(t).top[1] {
            t = self.split_at(rec.top, t);
            self.add_vertex(rec.top);
            lower = self.t(t).below[0].unwrap();
            assert!(self.t(t).below[1].is_none(), "!topTrapezoid->below[1]");
            clear_side(self, t);
        }
        if self.t(lower).bottom[1] != bottom_y {
            let upper = self.split_at(rec.bottom, lower);
            self.add_vertex(rec.bottom);
            assert!(self.t(upper).below[1].is_none(), "!trapezoid->below[1]");
            let below = self.t(upper).below[0].unwrap();
            clear_side(self, below);
            return upper;
        }
        lower
    }

    /// `PathBuild_LinkTrapezoidsToPortals`.
    fn link_portals(&mut self) {
        if self.plane != 0 {
            let mut t = 0;
            while t < self.traps.len() {
                if self.traps[t].alive {
                    let mut cur = t;
                    if let Some(p) = self.t(cur).left.map(|s| self.subs[s].portal).filter(|&p| p != 0xFFFF) {
                        cur = self.insert_edge_endpoints(p, 0, cur);
                    }
                    if let Some(p) = self.t(cur).right.map(|s| self.subs[s].portal).filter(|&p| p != 0xFFFF) {
                        self.insert_edge_endpoints(p, 1, cur);
                    }
                }
                t += 1;
            }
        }
        let live: Vec<T> = self.live().collect();
        for t in live {
            let pl = self.t(t).left.map_or(0xFFFF, |s| self.subs[s].portal);
            let pr = self.t(t).right.map_or(0xFFFF, |s| self.subs[s].portal);
            self.tm(t).portal = [pl, pr];
            // A left edge portal has the trapezoid on its right side.
            if pl != 0xFFFF {
                self.portals[pl as usize].lists[1].insert(0, t);
            }
            if pr != 0xFFFF {
                self.portals[pr as usize].lists[0].insert(0, t);
            }
        }
        let mut i = 0;
        while i < self.portals.len() {
            if !self.failed {
                let (left, right) = (!self.portals[i].lists[0].is_empty(), !self.portals[i].lists[1].is_empty());
                match (left, right) {
                    (false, true) => self.sort_and_create(i, 1),
                    (true, false) => self.sort_and_create(i, 0),
                    (true, true) => {
                        if self.portals[i].neighbor_plane == self.plane {
                            // Split into one portal per side.
                            let new_index = self.portals.len() as u16;
                            let mut copy = self.portals[i].clone();
                            copy.lists[0].clear();
                            self.portals[i].lists[1].clear();
                            let flags = self.portals[i].flags;
                            if flags & 3 != 0 && flags & 3 != 3 {
                                if flags & 1 != 0 {
                                    copy.flags = 4;
                                }
                                if flags & 2 != 0 {
                                    self.portals[i].flags = 4;
                                }
                            }
                            for &t in &copy.lists[1] {
                                self.tm(t).portal[0] = new_index;
                            }
                            self.portals.push(copy);
                            continue;
                        }
                        self.failed = true;
                    }
                    (false, false) => {}
                }
            } else {
                self.portals[i].lists = [Vec::new(), Vec::new()];
            }
            i += 1;
        }
    }

    /// `PathBuild_SortAndCreatePortals`.
    fn sort_and_create(&mut self, portal: usize, side: usize) {
        let start = self.portal_traps.len();
        let list = std::mem::take(&mut self.portals[portal].lists[side]);
        self.portal_traps.extend(list);
        if self.portal_traps.len() == start {
            return;
        }
        let mut range: Vec<T> = self.portal_traps[start..].to_vec();
        quicksort_desc(&mut range, |t| self.traps[t].top);
        self.portal_traps.truncate(start);
        self.portal_traps.push(range[0]);
        let mut last = range[0];
        for &t in &range[1..] {
            if !approx(self.t(t).top[1], self.t(last).bottom[1]) {
                break;
            }
            self.portal_traps.push(t);
            last = t;
        }
        let first = range[0];
        let count = (self.portal_traps.len() - start) as u32;
        self.portals[portal].ranges[side] = (count, start as u32);
        if self.portals[portal].neighbor_plane != self.plane {
            let key = (self.plane as u32) << 16 | self.portals[portal].id as u32;
            let (f, l) = (self.t(first), self.t(last));
            let (tx, bx) = if side == 1 { (f.top_x[0], l.bot_x[0]) } else { (f.top_x[1], l.bot_x[1]) };
            let rec = PortalRecord { top: [tx, f.top[1]], bottom: [bx, l.bottom[1]] };
            self.records.entry(key).or_insert(rec);
        }
    }

    // ----- output --------------------------------------------------------

    fn serialize(&mut self, start_points: &[[f64; 2]]) -> PathPlane {
        let mut trapezoids = Vec::new();
        let live: Vec<T> = self.live().collect();
        for (i, &t) in live.iter().enumerate() {
            self.tm(t).index = i as u32;
        }
        let idx = |b: &Builder, t: Option<T>| t.map_or(navmesh::NONE_U32, |t| b.t(t).index);
        for &t in &live {
            let tr = self.t(t).clone();
            let (mut yt, yb) = (tr.top[1] as f32, tr.bottom[1] as f32);
            let (xtl, mut xtr) = (tr.top_x[0] as f32, tr.top_x[1] as f32);
            let (xbl, mut xbr) = (tr.bot_x[0] as f32, tr.bot_x[1] as f32);
            if xtr < xtl {
                xtr = xtl;
            }
            if xbr < xbl {
                xbr = xbl;
            }
            if yt < yb {
                yt = yb;
            }
            if xtl == f32::NEG_INFINITY || xtr == f32::INFINITY || xbl == f32::NEG_INFINITY || xbr == f32::INFINITY
                || yt == f32::INFINITY || yb == f32::NEG_INFINITY
            {
                self.failed = true;
            }
            trapezoids.push(navmesh::Trapezoid {
                neighbors: [idx(self, tr.above[0]), idx(self, tr.above[1]), idx(self, tr.below[0]), idx(self, tr.below[1])],
                portal_left: tr.portal[0],
                portal_right: tr.portal[1],
                y_top: yt,
                y_bottom: yb,
                x_top_left: xtl,
                x_top_right: xtr,
                x_bottom_left: xbl,
                x_bottom_right: xbr,
            });
        }

        // PathChunk_WriteTreeStructure: depth first, right child first.
        let root = self.root.expect("root");
        let (mut xs, mut ys, mut sinks) = (Vec::new(), Vec::new(), Vec::new());
        let (mut xi, mut yi, mut si) = (0u32, 0x4000_0000u32, 0x8000_0000u32);
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if self.nodes[n].flags & WRITTEN != 0 {
                continue;
            }
            match self.nodes[n].kind {
                Kind::X { left, right, .. } => {
                    self.nodes[n].index = xi;
                    xi += 1;
                    stack.extend(left);
                    stack.extend(right);
                    xs.push(n);
                }
                Kind::Y { above, below, .. } => {
                    self.nodes[n].index = yi;
                    yi += 1;
                    stack.extend(above);
                    stack.extend(below);
                    ys.push(n);
                }
                Kind::Sink(_) => {
                    self.nodes[n].index = si;
                    si += 1;
                    sinks.push(n);
                }
            }
            self.nodes[n].flags |= WRITTEN;
        }
        let nref = |b: &Builder, n: Option<N>| NodeRef::from_raw(n.map_or(navmesh::NONE_U32, |n| b.nodes[n].index));
        let x_nodes = xs
            .iter()
            .map(|&n| {
                let Kind::X { seg, left, right } = self.nodes[n].kind else { unreachable!() };
                navmesh::XNode {
                    v0: self.vertex_of(seg.p0),
                    v1: self.vertex_of(seg.p1),
                    left: nref(self, left),
                    right: nref(self, right),
                }
            })
            .collect();
        let y_nodes = ys
            .iter()
            .map(|&n| {
                let Kind::Y { p, above, below } = self.nodes[n].kind else { unreachable!() };
                navmesh::YNode { v: self.vertex_of(p), above: nref(self, above), below: nref(self, below) }
            })
            .collect();
        let sinks = sinks
            .iter()
            .map(|&n| {
                let Kind::Sink(t) = self.nodes[n].kind else { unreachable!() };
                self.t(t).index
            })
            .collect();
        let portals = self
            .portals
            .iter()
            .map(|p| {
                let (count, start) = if p.ranges[0].0 != 0 { p.ranges[0] } else { p.ranges[1] };
                navmesh::Portal {
                    count: count as u16,
                    start: start as u16,
                    neighbor_plane: p.neighbor_plane,
                    pair: p.id,
                    flags: p.flags,
                }
            })
            .collect();
        PathPlane {
            start_points: start_points.iter().map(|p| [p[0] as f32, p[1] as f32]).collect(),
            vectors: self.vertices.iter().map(|v| [v[0] as f32, v[1] as f32]).collect(),
            trapezoids,
            root: NodeRef::from_raw(self.nodes[root].index),
            x_nodes,
            y_nodes,
            sinks,
            portal_trapezoids: self.portal_traps.iter().map(|&t| self.t(t).index).collect(),
            portals,
        }
    }
}

/// The client's quicksort (explicit stack, middle element as pivot) in
/// descending `(y, x)` order of the trapezoids' top points.
fn quicksort_desc(v: &mut [T], key: impl Fn(T) -> [f64; 2]) {
    // "a after b": a.y < b.y, or a.y <= b.y and a.x < b.x.
    let after = |a: [f64; 2], b: [f64; 2]| a[1] < b[1] || (a[1] <= b[1] && a[0] < b[0]);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let (mut lo, mut hi) = (0usize, v.len());
    if hi - lo < 2 {
        return;
    }
    loop {
        let mid = lo + (hi - lo) / 2;
        v.swap(lo, mid);
        let pivot = key(v[lo]);
        let (mut i, mut j) = (lo, hi);
        loop {
            i += 1;
            while i != hi && !after(key(v[i]), pivot) {
                i += 1;
            }
            j -= 1;
            while j != lo && !after(pivot, key(v[j])) {
                j -= 1;
            }
            if j < i {
                break;
            }
            v.swap(i, j);
        }
        v.swap(lo, j);
        if j - lo < hi - i {
            if i + 1 < hi {
                stack.push((i, hi));
            }
            if lo + 1 < j {
                hi = j;
                continue;
            }
        } else {
            if lo + 1 < j {
                stack.push((lo, j));
            }
            if i + 1 < hi {
                lo = i;
                continue;
            }
        }
        match stack.pop() {
            Some((a, b)) => (lo, hi) = (a, b),
            None => return,
        }
    }
}

/// Build one plane (`PathMap_Build`).
pub fn build_plane(plane: u16, input: &PlaneInput, records: &mut PortalRecords) -> PathPlane {
    let mut b = Builder {
        plane,
        subs: Vec::new(),
        traps: Vec::new(),
        nodes: Vec::new(),
        root: None,
        vertices: Vec::new(),
        vertex_index: HashMap::new(),
        portals: Vec::new(),
        portal_traps: Vec::new(),
        failed: false,
        records,
    };
    // The initial unbounded trapezoid.
    let t = b.new_trap();
    {
        let tr = b.tm(t);
        tr.top_x = [f64::NEG_INFINITY, f64::INFINITY];
        tr.top = [f64::INFINITY, f64::INFINITY];
        tr.bot_x = [f64::NEG_INFINITY, f64::INFINITY];
        tr.bottom = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    }
    b.root = Some(b.new_sink(t));

    let mut clipper = Clipper::default();
    for seg in &input.segments {
        b.insert(seg, &mut clipper);
    }
    while b.remove_degenerate() {}
    let starts: Vec<[f64; 2]> = input.start_points.iter().map(|p| [p.x, p.y]).collect();
    b.mark_reachable(&starts);
    let root = b.root;
    b.mark_dead(root);
    b.prune();
    b.portals = input
        .portals
        .iter()
        .map(|p: &PortalInput| BPortal { neighbor_plane: p.neighbor_plane, id: p.portal, flags: p.side_flags, ..Default::default() })
        .collect();
    b.link_portals();
    b.serialize(&starts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathgen::assemble::assemble;
    use crate::pathgen::testing::Fixture;

    fn first_diff<A: PartialEq + std::fmt::Debug>(what: &str, ours: &[A], theirs: &[A]) -> Option<String> {
        if ours == theirs {
            return None;
        }
        let i = ours.iter().zip(theirs).take_while(|(a, b)| a == b).count();
        Some(format!(
            "{what}: {} vs {}, first difference at {i}\n     ours   {:?}\n     client {:?}",
            ours.len(),
            theirs.len(),
            ours.get(i),
            theirs.get(i)
        ))
    }

    #[test]
    fn planes_match_client() {
        for f in Fixture::all() {
            let inputs = assemble(&f.traced.segments, &f.start_points, &f.props.points);
            assert_eq!(inputs.len(), f.client_planes.len(), "{:?}: plane count", f.pair);
            let mut records = PortalRecords::new();
            let mut bad = 0;
            let mut counts = [0; 5];
            for (i, (input, client)) in inputs.iter().zip(&f.client_planes).enumerate() {
                let ours = build_plane(i as u16, input, &mut records);
                counts[0] += ours.trapezoids.len();
                counts[1] += ours.x_nodes.len();
                counts[2] += ours.y_nodes.len();
                counts[3] += ours.portals.len();
                counts[4] += ours.portal_trapezoids.len();
                let diffs: Vec<String> = [
                    first_diff("start points", &ours.start_points, &client.start_points),
                    first_diff("vectors", &ours.vectors, &client.vectors),
                    first_diff("trapezoids", &ours.trapezoids, &client.trapezoids),
                    first_diff("root", &[ours.root], &[client.root]),
                    first_diff("x nodes", &ours.x_nodes, &client.x_nodes),
                    first_diff("y nodes", &ours.y_nodes, &client.y_nodes),
                    first_diff("sinks", &ours.sinks, &client.sinks),
                    first_diff("portal trapezoids", &ours.portal_trapezoids, &client.portal_trapezoids),
                    first_diff("portals", &ours.portals, &client.portals),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !diffs.is_empty() {
                    bad += 1;
                    eprintln!("{:?} plane {i}:\n  {}", f.pair, diffs.join("\n  "));
                }
            }
            eprintln!(
                "{:?}: {} of {} planes equal ({} trapezoids, {} x / {} y nodes, {} portals, {} portal trapezoids)",
                f.pair,
                inputs.len() - bad,
                inputs.len(),
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4]
            );
            assert_eq!(bad, 0, "{:?}", f.pair);
        }
    }
}
