//! Terrain seen from above: the client's texture splatting, hill-shaded,
//! with water over the parts below the water surface.
//!
//! The splatting follows GuildWarsMapBrowser (`Terrain.cpp`,
//! `TerrainRevPixelShader.hlsl`). Each terrain texture is a 256x256 tile of
//! four 128x128 quadrants whose alpha holds transition masks. A cell's
//! corners name up to four textures; the lowest-numbered one is drawn
//! opaque from a pseudo-random quadrant, and the others are blended over it
//! from the quadrant (and 180-degree rotation) that matches which corners
//! they cover.

use crate::mapfile::terrain::{CHUNK_SIZE, TerrainStrip, TerrainSurface, XY_DIST};

use super::atex::Texture;
use super::canvas::{Canvas, par_rows};

const QUADRANT: f32 = 128.0;
/// Inset of a cell's texture coordinates inside its quadrant.
const BORDER: f32 = 8.5;

/// Per corner mask (TL 1, TR 2, BL 4, BR 8): the quadrant variant for the
/// texture covering those corners, and a second variant drawn after it when
/// a cell has exactly two textures. Bit `0x8000` is a 180-degree rotation.
const VARIANTS: [(u16, Option<u16>); 16] = [
    (0x8000, Some(0x0000)),
    (0x8003, None),
    (0x0001, None),
    (0x8000, None),
    (0x8001, None),
    (0x0002, None),
    (0x8001, Some(0x0001)),
    (0x0002, Some(0x0001)),
    (0x0003, None),
    (0x8003, Some(0x0003)),
    (0x8002, None),
    (0x8000, Some(0x0003)),
    (0x0000, None),
    (0x0000, Some(0x8003)),
    (0x0000, Some(0x0001)),
    (0x8002, Some(0x0002)),
];

/// A terrain texture and its mip levels (box-filtered).
#[derive(Clone)]
pub struct Mipmapped {
    pub levels: Vec<Texture>,
}

impl Mipmapped {
    pub fn new(base: Texture) -> Self {
        let mut levels = vec![base];
        while let Some(last) = levels.last().filter(|t| t.width > 1 && t.height > 1) {
            let (w, h) = (last.width / 2, last.height / 2);
            let mut pixels = Vec::with_capacity(w * h);
            for y in 0..h {
                for x in 0..w {
                    let p = [last.pixel(2 * x, 2 * y), last.pixel(2 * x + 1, 2 * y), last.pixel(2 * x, 2 * y + 1), last.pixel(2 * x + 1, 2 * y + 1)];
                    pixels.push(std::array::from_fn(|i| ((p.iter().map(|q| q[i] as u32).sum::<u32>() + 2) / 4) as u8));
                }
            }
            levels.push(Texture { width: w, height: h, pixels });
        }
        Self { levels }
    }
}

/// The layers of a cell, at most three.
type CellLayers = [Option<Layer>; 3];

/// One texture layer of a cell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layer {
    texture: u8,
    quadrant: u8,
    rotated: bool,
}

/// The client's per-chunk random generator (Park-Miller, 48271).
fn next_random(state: &mut u32) -> u32 {
    let mult = state.wrapping_mul(48271);
    let correction = (*state / 44488).wrapping_mul(0x7FFF_FFFF);
    let mut v = mult.wrapping_sub(correction);
    if v > 0x7FFF_FFFF {
        v = v.wrapping_add(0x8000_0000);
    }
    if v == 0 {
        v = 123_459_876;
    }
    *state = v;
    v
}

/// The layers of a cell, given its corner textures (TL, TR, BL, BR) and the
/// random quadrant of the base texture. At most three are drawn.
fn cell_layers(corners: [u8; 4], random_quadrant: u8) -> CellLayers {
    let mut out = [None; 3];
    let base = |texture| Layer { texture, quadrant: random_quadrant, rotated: false };
    if corners.iter().all(|&t| t == corners[0]) {
        out[0] = Some(base(corners[0]));
        return out;
    }
    let mut textures: Vec<(u8, usize)> = Vec::with_capacity(4);
    for (bit, &t) in corners.iter().enumerate() {
        match textures.iter_mut().find(|(tex, _)| *tex == t) {
            Some((_, mask)) => *mask |= 1 << bit,
            None => textures.push((t, 1 << bit)),
        }
    }
    textures.sort_unstable_by_key(|(t, _)| *t);
    let variant = |texture, v: u16| Layer { texture, quadrant: (v & 3) as u8, rotated: v & 0x8000 != 0 };
    let mut layers = Vec::with_capacity(4);
    for (i, &(texture, mask)) in textures.iter().enumerate() {
        layers.push(if i == 0 { base(texture) } else { variant(texture, VARIANTS[mask].0) });
    }
    if let [_, (texture, mask)] = textures[..]
        && let Some(v) = VARIANTS[mask].1
    {
        layers.push(variant(texture, v));
    }
    for (slot, layer) in out.iter_mut().zip(layers) {
        *slot = Some(layer);
    }
    out
}

/// Light direction (towards the light): from the north-west, 50 degrees up.
const LIGHT: [f32; 3] = [-0.454_519, 0.454_519, 0.766_044];
const AMBIENT: f32 = 0.62;
const DIFFUSE: f32 = 0.5;

/// Light factor for a surface normal (up-positive z; any length).
pub fn shade(n: [f32; 3]) -> f32 {
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
    AMBIENT + DIFFUSE * ((n[0] * LIGHT[0] + n[1] * LIGHT[1] + n[2] * LIGHT[2]) / len).max(0.0)
}

/// Water drawn over terrain below its surface.
#[derive(Debug, Clone, Copy)]
pub struct WaterStyle {
    /// Elevation (up-positive) of the surface.
    pub elevation: f32,
    /// Linear RGB, 0..1.
    pub color: [f32; 3],
}

/// Draw the terrain into `canvas`. `textures` are indexed like the terrain
/// file references; missing ones fall back to grey.
pub fn draw(
    canvas: &mut Canvas,
    terrain: &TerrainStrip,
    surface: &TerrainSurface,
    textures: &[Option<Mipmapped>],
    water: Option<WaterStyle>,
) {
    let [dim_x, dim_y] = terrain.header.dims;
    let chunks_x = dim_x / CHUNK_SIZE;
    let sample = |x: usize, y: usize| {
        let chunk = (y / CHUNK_SIZE) * chunks_x + x / CHUNK_SIZE;
        chunk * CHUNK_SIZE * CHUNK_SIZE + (y % CHUNK_SIZE) * CHUNK_SIZE + x % CHUNK_SIZE
    };
    let elevation = |x: usize, y: usize| -terrain.heights[sample(x, y)];
    let texture_at = |x: usize, y: usize| surface.textures[sample(x, y)];

    // Unit normals per sample, from central differences.
    let mut normals = vec![[0f32; 3]; dim_x * dim_y];
    for y in 0..dim_y {
        for x in 0..dim_x {
            let (x0, x1) = (x.saturating_sub(1), (x + 1).min(dim_x - 1));
            let (y0, y1) = (y.saturating_sub(1), (y + 1).min(dim_y - 1));
            // World y grows towards row 0.
            let dx = (elevation(x1, y) - elevation(x0, y)) / ((x1 - x0).max(1) as f32 * XY_DIST);
            let dy = (elevation(x, y0) - elevation(x, y1)) / ((y1 - y0).max(1) as f32 * XY_DIST);
            let len = (dx * dx + dy * dy + 1.0).sqrt();
            normals[y * dim_x + x] = [-dx / len, -dy / len, 1.0 / len];
        }
    }

    // The base texture's random quadrant per cell: one draw per cell of a
    // chunk, row by row, seeded per chunk.
    let mut random_quadrant = vec![0u8; dim_x * dim_y];
    for cz in 0..dim_y.div_ceil(CHUNK_SIZE) {
        for cx in 0..dim_x.div_ceil(CHUNK_SIZE) {
            let mut state = (cz as u32) ^ ((cx as u32) << 16);
            for lz in 0..CHUNK_SIZE {
                for lx in 0..CHUNK_SIZE {
                    let r = next_random(&mut state);
                    let (x, y) = (cx * CHUNK_SIZE + lx, cz * CHUNK_SIZE + lz);
                    if x < dim_x && y < dim_y {
                        random_quadrant[y * dim_x + x] = (r & 3) as u8;
                    }
                }
            }
        }
    }

    let view = canvas.view;
    // Texels of a quadrant's span per output pixel picks the mip level.
    let texels_per_pixel = (QUADRANT - 2.0 * BORDER) * view.scale / XY_DIST;
    let level = texels_per_pixel.max(1.0).log2().floor() as usize;
    let fallback: [f32; 3] = [0.45, 0.45, 0.42];
    let averages: Vec<[f32; 3]> = textures
        .iter()
        .map(|t| t.as_ref().map_or(fallback, |t| { let a = t.levels[0].average(); [a[0] / 255.0, a[1] / 255.0, a[2] / 255.0] }))
        .collect();

    par_rows(canvas, |py, colors, elevations| {
        let mut cached: Option<((usize, usize), CellLayers)> = None;
        for (px, (color, out_elevation)) in colors.iter_mut().zip(elevations.iter_mut()).enumerate() {
            let [wx, wy] = view.world(px, py);
            let gx = ((wx - view.min_x) / XY_DIST).clamp(0.0, (dim_x - 1) as f32);
            let gy = ((view.max_y - wy) / XY_DIST).clamp(0.0, (dim_y - 1) as f32);
            let (cx, cy) = ((gx as usize).min(dim_x - 2), (gy as usize).min(dim_y - 2));
            let (fx, fy) = (gx - cx as f32, gy - cy as f32);
            let bilerp = |a: f32, b: f32, c: f32, d: f32| (a * (1.0 - fx) + b * fx) * (1.0 - fy) + (c * (1.0 - fx) + d * fx) * fy;

            let layers = match cached {
                Some((cell, layers)) if cell == (cx, cy) => layers,
                _ => {
                    let corners = [texture_at(cx, cy), texture_at(cx + 1, cy), texture_at(cx, cy + 1), texture_at(cx + 1, cy + 1)];
                    let layers = cell_layers(corners, random_quadrant[cy * dim_x + cx]);
                    cached = Some(((cx, cy), layers));
                    layers
                }
            };
            let mut rgb = [0f32; 3];
            for (i, layer) in layers.iter().flatten().enumerate() {
                let (sample, alpha) = sample_layer(textures, &averages, layer, fx, fy, level);
                let a = if i == 0 { 1.0 } else { alpha };
                for c in 0..3 {
                    rgb[c] += (sample[c] - rgb[c]) * a;
                }
            }

            let n00 = normals[cy * dim_x + cx];
            let n10 = normals[cy * dim_x + cx + 1];
            let n01 = normals[(cy + 1) * dim_x + cx];
            let n11 = normals[(cy + 1) * dim_x + cx + 1];
            let light = shade(std::array::from_fn(|i| bilerp(n00[i], n10[i], n01[i], n11[i])));
            let e = bilerp(elevation(cx, cy), elevation(cx + 1, cy), elevation(cx, cy + 1), elevation(cx + 1, cy + 1));
            let mut out = rgb.map(|v| v * light);
            if let Some(w) = water.filter(|w| e < w.elevation) {
                // Deeper water hides more of the ground.
                let depth = w.elevation - e;
                let a = (0.55 + depth / 1500.0).min(0.9);
                for c in 0..3 {
                    out[c] += (w.color[c] - out[c]) * a;
                }
            }
            *color = out;
            *out_elevation = e.max(water.map_or(f32::NEG_INFINITY, |w| w.elevation));
        }
    });
}

/// Sample one layer at cell position (`fx`, `fy`); returns linear RGB in
/// 0..1 and the alpha.
fn sample_layer(textures: &[Option<Mipmapped>], averages: &[[f32; 3]], layer: &Layer, fx: f32, fy: f32, level: usize) -> ([f32; 3], f32) {
    let Some(mips) = textures.get(layer.texture as usize).and_then(Option::as_ref) else {
        let avg = averages.get(layer.texture as usize).copied().unwrap_or([0.45, 0.45, 0.42]);
        return (avg, 1.0);
    };
    let level = level.min(mips.levels.len() - 1);
    let tex = &mips.levels[level];
    let scale = tex.width as f32 / 256.0;
    let (u, v) = if layer.rotated { (1.0 - fx, 1.0 - fy) } else { (fx, fy) };
    let (qx, qy) = ([0.0, QUADRANT, 0.0, QUADRANT][layer.quadrant as usize], [0.0, 0.0, QUADRANT, QUADRANT][layer.quadrant as usize]);
    let span = QUADRANT - 2.0 * BORDER;
    // Texel coordinates, kept inside the quadrant.
    let lo = 0.5;
    let hi = (QUADRANT * scale - 0.5).max(lo);
    let tx = ((BORDER + u * span) * scale).clamp(lo, hi) + qx * scale;
    let ty = ((BORDER + v * span) * scale).clamp(lo, hi) + qy * scale;
    let s = tex.sample_wrapped(tx, ty);
    ([s[0] / 255.0, s[1] / 255.0, s[2] / 255.0], s[3] / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_matches_client_sequence() {
        // Seed 0 is replaced by the generator's fallback value.
        let mut s = 0;
        assert_eq!(next_random(&mut s), 123_459_876);
        let mut s = 1;
        assert_eq!(next_random(&mut s), 48271);
    }

    #[test]
    fn layers_for_two_textures() {
        // TL and TR are texture 5, BL and BR texture 2: 2 is the base, 5
        // covers the top corners (mask 3).
        let layers = cell_layers([5, 5, 2, 2], 1);
        assert_eq!(layers[0], Some(Layer { texture: 2, quadrant: 1, rotated: false }));
        assert_eq!(layers[1], Some(Layer { texture: 5, quadrant: 0, rotated: true }));
        assert_eq!(layers[2], None);
    }
}
