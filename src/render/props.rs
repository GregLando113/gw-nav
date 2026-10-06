//! Props seen from above: each prop's model placed like its collision
//! outline (`pathgen::props`, but in float without the rounding), then
//! rasterized on its own with a depth test against the terrain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::mapfile::props::PropDef;

use super::canvas::{Canvas, View, threads};
use super::model::Model;
use super::terrain::{Mipmapped, shade};

/// Alpha below which a texel is cut out (foliage, fences).
const ALPHA_CUTOFF: f32 = 0.5;
/// Colour of untextured surfaces (linear RGB).
const UNTEXTURED: [f32; 3] = [0.55, 0.55, 0.52];

/// A prop's placement: world = rotate(model.xy) * scale + position; the
/// model's z (down) is scaled and added to the position's z.
#[derive(Debug, Clone, Copy)]
struct Placement {
    sin: f32,
    cos: f32,
    scale: f32,
    position: [f32; 3],
}

impl Placement {
    fn new(prop: &PropDef) -> Self {
        let scale = prop.scale as f32 * (1.992_187_5 / 256.0) + 0.007_812_5;
        // `Prop_calc_rotation_vectors`: the first vector's x and y.
        let a = prop.rotation.map(|b| b as f32 * (std::f32::consts::TAU / 256.0));
        let sin = a[2].sin() * a[1].cos() - a[2].cos() * a[0].sin() * a[1].sin();
        let cos = a[2].cos() * a[0].cos();
        // The yaw vector shrinks when the prop is tilted; keep its direction.
        let len = (sin * sin + cos * cos).sqrt().max(1e-6);
        Self { sin: sin / len, cos: cos / len, scale, position: prop.position }
    }

    /// World x, y and up-positive elevation.
    fn apply(&self, [mx, my, mz]: [f32; 3]) -> [f32; 3] {
        let [px, py, pz] = self.position;
        [
            (self.cos * mx + self.sin * my) * self.scale + px,
            (self.cos * my - self.sin * mx) * self.scale + py,
            -(pz + mz * self.scale),
        ]
    }
}

struct Instance<'a> {
    model: &'a Model,
    placement: Placement,
    /// Pixel bounding box: x0, y0, x1, y1.
    bbox: [f32; 4],
}

/// One prop drawn on its own over the terrain: the pixels where it is above
/// the terrain. Composited with the other props by elevation, the sprites
/// give the same picture as drawing every prop into one canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct Sprite {
    /// Index of the prop in the props chunk.
    pub prop: usize,
    /// Pixel rectangle in the canvas: x, y, width, height.
    pub rect: [usize; 4],
    /// Linear RGB per pixel, row-major; the terrain's colour where the prop
    /// isn't drawn.
    pub color: Vec<[f32; 3]>,
    /// Up-positive elevation per pixel; `NEG_INFINITY` where the prop isn't
    /// drawn.
    pub elevation: Vec<f32>,
}

/// Draw each of `props` on its own over the terrain in `canvas`, whose
/// elevations are the depth test. `models` and `textures` are by base file
/// id; `model_refs` maps a prop's model index to a base file id. Props that
/// draw nothing get no sprite; the others come in prop order.
pub fn rasterize(
    canvas: &Canvas,
    props: &[PropDef],
    model_refs: &[u32],
    models: &HashMap<u32, Model>,
    textures: &HashMap<u32, Mipmapped>,
) -> Vec<Sprite> {
    let view = canvas.view;
    let instances: Vec<(usize, Instance)> = props
        .iter()
        .enumerate()
        .filter_map(|(i, prop)| {
            let model = models.get(model_refs.get(prop.model as usize)?)?;
            let placement = Placement::new(prop);
            let mut bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for p in model.submeshes.iter().flat_map(|s| &s.positions) {
                let [x, y, _] = placement.apply(*p);
                let [px, py] = view.pixel([x, y]);
                bbox = [bbox[0].min(px), bbox[1].min(py), bbox[2].max(px), bbox[3].max(py)];
            }
            (bbox[0] <= bbox[2]).then_some((i, Instance { model, placement, bbox }))
        })
        .collect();

    // Props differ a lot in size, so threads take them one at a time.
    let next = AtomicUsize::new(0);
    let work = || {
        let mut out = Vec::new();
        while let Some((prop, inst)) = instances.get(next.fetch_add(1, Ordering::Relaxed)) {
            out.extend(sprite(canvas, *prop, inst, textures));
        }
        out
    };
    let mut sprites: Vec<Sprite> = match threads() {
        1 => work(),
        n => std::thread::scope(|s| {
            let handles: Vec<_> = (0..n).map(|_| s.spawn(work)).collect();
            handles.into_iter().flat_map(|h| h.join().expect("prop rasterizer thread")).collect()
        }),
    };
    sprites.sort_by_key(|s| s.prop);
    sprites
}

/// Rasterize one prop over the terrain, trimmed to the pixels it covers.
fn sprite(canvas: &Canvas, prop: usize, inst: &Instance, textures: &HashMap<u32, Mipmapped>) -> Option<Sprite> {
    let view = canvas.view;
    let x0 = inst.bbox[0].floor().max(0.0) as usize;
    let y0 = inst.bbox[1].floor().max(0.0) as usize;
    let x1 = (inst.bbox[2].ceil() as isize + 1).clamp(0, view.width as isize) as usize;
    let y1 = (inst.bbox[3].ceil() as isize + 1).clamp(0, view.height as isize) as usize;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let canvas_index = |x: usize, y: usize| (y0 + y) * view.width + x0 + x;
    let mut target = Target {
        view,
        rect: [x0, y0, w, h],
        colors: vec![[0.0; 3]; w * h],
        elevations: (0..h).flat_map(|y| (0..w).map(move |x| canvas.elevation[canvas_index(x, y)])).collect(),
    };
    for sub in &inst.model.submeshes {
        let texture = sub.texture.and_then(|slot| {
            let id = inst.model.textures.get(slot.texture as usize)?;
            Some((textures.get(id)?, sub.uvs.get(slot.uv_set as usize).or(sub.uvs.first())?))
        });
        let world: Vec<[f32; 3]> = sub.positions.iter().map(|p| inst.placement.apply(*p)).collect();
        for tri in sub.indices.chunks_exact(3) {
            let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            let uv = texture.map(|(_, uvs)| [uvs[a], uvs[b], uvs[c]]);
            target.triangle([world[a], world[b], world[c]], texture.map(|(t, _)| t), uv);
        }
    }

    // The prop's pixels are where it rose above the terrain.
    let drawn = |x: usize, y: usize| target.elevations[y * w + x] > canvas.elevation[canvas_index(x, y)];
    let (mut tx0, mut ty0, mut tx1, mut ty1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..h {
        for x in (0..w).filter(|&x| drawn(x, y)) {
            (tx0, ty0, tx1, ty1) = (tx0.min(x), ty0.min(y), tx1.max(x + 1), ty1.max(y + 1));
        }
    }
    if tx0 >= tx1 {
        return None;
    }
    let (tw, th) = (tx1 - tx0, ty1 - ty0);
    let mut color = Vec::with_capacity(tw * th);
    let mut elevation = Vec::with_capacity(tw * th);
    for y in ty0..ty1 {
        for x in tx0..tx1 {
            if drawn(x, y) {
                color.push(target.colors[y * w + x]);
                elevation.push(target.elevations[y * w + x]);
            } else {
                color.push(canvas.color[canvas_index(x, y)]);
                elevation.push(f32::NEG_INFINITY);
            }
        }
    }
    Some(Sprite { prop, rect: [x0 + tx0, y0 + ty0, tw, th], color, elevation })
}

/// The pixels being drawn into: `rect` is x, y, width and height in the
/// canvas, and the buffers hold exactly its pixels.
struct Target {
    view: View,
    rect: [usize; 4],
    colors: Vec<[f32; 3]>,
    elevations: Vec<f32>,
}

impl Target {
    fn triangle(&mut self, world: [[f32; 3]; 3], texture: Option<&Mipmapped>, uv: Option<[[f32; 2]; 3]>) {
        let p = world.map(|w| self.view.pixel([w[0], w[1]]));
        let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
        if area.abs() < 1e-9 {
            return;
        }
        // Flat shading with the normal turned upwards (roofless interiors
        // and backfaces are seen from above too).
        let (e1, e2) = (sub3(world[1], world[0]), sub3(world[2], world[0]));
        let mut n = cross(e1, e2);
        if n[2] < 0.0 {
            n = n.map(|v| -v);
        }
        let light = shade(n);

        let [rx, ry, rw, rh] = self.rect;
        let x_min = p.iter().map(|q| q[0]).fold(f32::MAX, f32::min).floor().max(rx as f32) as usize;
        let x_max = (p.iter().map(|q| q[0]).fold(f32::MIN, f32::max).ceil() as isize).min((rx + rw) as isize - 1);
        let y_min = p.iter().map(|q| q[1]).fold(f32::MAX, f32::min).floor().max(ry as f32) as usize;
        let y_max = (p.iter().map(|q| q[1]).fold(f32::MIN, f32::max).ceil() as isize).min((ry + rh) as isize - 1);
        if x_max < x_min as isize || y_max < y_min as isize {
            return;
        }

        // Mip level from texels per pixel.
        let level = match (texture, uv) {
            (Some(t), Some(uv)) => {
                let base = &t.levels[0];
                let (w, h) = (base.width as f32, base.height as f32);
                let tex_area = ((uv[1][0] - uv[0][0]) * w * (uv[2][1] - uv[0][1]) * h
                    - (uv[2][0] - uv[0][0]) * w * (uv[1][1] - uv[0][1]) * h)
                    .abs();
                (0.5 * (tex_area / area.abs()).max(1.0).log2()).floor() as usize
            }
            _ => 0,
        };

        let inv = 1.0 / area;
        for py in y_min..=y_max as usize {
            let row = (py - ry) * rw;
            let cy = py as f32 + 0.5;
            for px in x_min..=x_max as usize {
                let cx = px as f32 + 0.5;
                let w0 = ((p[1][0] - cx) * (p[2][1] - cy) - (p[2][0] - cx) * (p[1][1] - cy)) * inv;
                let w1 = ((p[2][0] - cx) * (p[0][1] - cy) - (p[0][0] - cx) * (p[2][1] - cy)) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let e = w0 * world[0][2] + w1 * world[1][2] + w2 * world[2][2];
                let i = row + px - rx;
                if e <= self.elevations[i] {
                    continue;
                }
                let rgb = match (texture, uv) {
                    (Some(t), Some(uv)) => {
                        let tex = &t.levels[level.min(t.levels.len() - 1)];
                        let u = w0 * uv[0][0] + w1 * uv[1][0] + w2 * uv[2][0];
                        let v = w0 * uv[0][1] + w1 * uv[1][1] + w2 * uv[2][1];
                        let s = tex.sample_wrapped(u * tex.width as f32, v * tex.height as f32);
                        if s[3] / 255.0 < ALPHA_CUTOFF {
                            continue;
                        }
                        [s[0] / 255.0, s[1] / 255.0, s[2] / 255.0]
                    }
                    _ => UNTEXTURED,
                };
                self.colors[i] = rgb.map(|c| c * light);
                self.elevations[i] = e;
            }
        }
    }
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::model::Submesh;

    /// A flat untextured square of side `2 * half` at elevation `z` (up),
    /// tilted so its elevation rises by `tilt` per unit of x.
    fn square(half: f32, z: f32, tilt: f32) -> Model {
        let corners = [[-half, -half], [half, -half], [half, half], [-half, half]];
        Model {
            submeshes: vec![Submesh {
                indices: vec![0, 1, 2, 0, 2, 3],
                // Model z points down.
                positions: corners.iter().map(|&[x, y]| [x, y, -(z + tilt * x)]).collect(),
                uvs: Vec::new(),
                texture: None,
            }],
            textures: Vec::new(),
        }
    }

    fn prop(model: u16, x: f32, y: f32) -> PropDef {
        // Scale byte 128 is a scale of about 1.
        PropDef { model, position: [x, y, 0.0], rotation: [0; 3], scale: 128, flags: 0, points: Vec::new() }
    }

    /// Two crossing tilted squares over flat terrain: the per-prop sprites
    /// composited by elevation match drawing both into one depth buffer.
    #[test]
    fn sprites_composite_like_one_pass() {
        let view = View::fit([0.0, 0.0, 64.0, 64.0], 1.0, 1024);
        let mut canvas = Canvas::new(view);
        canvas.color.iter_mut().for_each(|c| *c = [0.2, 0.4, 0.2]);
        canvas.elevation.iter_mut().for_each(|e| *e = 0.0);
        let models = HashMap::from([(10, square(16.0, 10.0, 0.5)), (11, square(16.0, 10.0, -0.5))]);
        let props = [prop(0, 24.0, 32.0), prop(1, 40.0, 32.0), prop(2, 8.0, 8.0)];
        let refs = [10, 11, 99];
        let sprites = rasterize(&canvas, &props, &refs, &models, &HashMap::new());
        // The third prop's model is missing.
        assert_eq!(sprites.iter().map(|s| s.prop).collect::<Vec<_>>(), [0, 1]);

        // One pass: both sprites' pixels into a shared depth buffer.
        let mut both = canvas.clone();
        for s in &sprites {
            let [x0, y0, w, _] = s.rect;
            for (k, (&c, &e)) in s.color.iter().zip(&s.elevation).enumerate() {
                let i = (y0 + k / w) * view.width + x0 + k % w;
                if e > both.elevation[i] {
                    both.color[i] = c;
                    both.elevation[i] = e;
                }
            }
        }
        // Where the squares overlap, each wins on the side it rises on.
        let at = |x: usize| both.elevation[32 * view.width + x];
        assert!(at(30) > 10.0 && at(34) > 10.0);
        let owner = |x: usize| sprites.iter().position(|s| {
            let [x0, y0, w, _] = s.rect;
            x >= x0 && x < x0 + w && s.elevation[(32 - y0) * w + x - x0] == at(x)
        });
        assert_eq!((owner(30), owner(34)), (Some(1), Some(0)));
        // Sprites are trimmed to their pixels.
        assert!(sprites.iter().all(|s| s.rect[2] <= 34 && s.rect[3] <= 34));
    }
}
