//! Bake a map's top-down render, and the render's file format.
//!
//! The terrain (with water) is one image. Each prop is a sprite of its own,
//! so a viewer can hide props: [`WorldRender::compose`] puts the visible
//! sprites over the terrain by elevation.

use std::collections::{HashMap, HashSet};

use crate::mapfile::environment::Environment;
use crate::mapfile::params::MapParams;
use crate::mapfile::props::PropsStrip;
use crate::mapfile::terrain::TerrainStrip;
use crate::mapfile::{Ffna, MapFileError, parse_file_refs};

use super::canvas::{Canvas, View, rgb8};
use super::model::{self, Model};
use super::terrain::{self, Mipmapped, WaterStyle};
use super::{RenderError, Result, atex, props};

/// World units per pixel of the baked image.
pub const SCALE: f32 = 12.0;
/// Largest image side in pixels; bigger maps get a coarser scale.
pub const MAX_SIDE: usize = 8192;
/// JPEG quality of the stored images.
const QUALITY: u8 = 85;
/// Finest elevation step between props, in world units.
const DEPTH_STEP: f32 = 1.0;

/// The files a map's render needs besides the map file itself, by base
/// file id: the terrain textures and the prop models. The models'
/// textures follow from [`required_model_textures`].
pub fn required_files(map: &[u8]) -> Result<Vec<u32>> {
    let file = Ffna::parse(map)?;
    let mut ids = Vec::new();
    for refs in [0x1100_0002, 0x1100_0004] {
        if let Some(refs) = file.chunk(refs) {
            ids.extend(parse_file_refs(refs)?);
        }
    }
    Ok(dedup(ids))
}

/// The textures of the models among `files` (by base file id), less those
/// already in `files`.
pub fn required_model_textures(files: &HashMap<u32, Vec<u8>>) -> Vec<u32> {
    let mut ids = Vec::new();
    for data in files.values() {
        let Ok(ffna) = Ffna::parse(data) else { continue };
        if ffna.file_type == 2
            && let Some(refs) = ffna.chunk(model::TEXTURES_CHUNK)
        {
            ids.extend(model::texture_refs(refs).unwrap_or_default());
        }
    }
    ids.retain(|id| !files.contains_key(id));
    dedup(ids)
}

fn dedup(mut ids: Vec<u32>) -> Vec<u32> {
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(*id));
    ids
}

/// Render a map at the stored scale. `files` holds the files named by
/// [`required_files`] and [`required_model_textures`] by base id; missing
/// ones are drawn as a plain colour or left out.
pub fn bake(map: &[u8], files: &HashMap<u32, Vec<u8>>) -> Result<WorldRender> {
    bake_with(map, files, SCALE, MAX_SIDE)
}

/// [`bake`] at `scale` world units per pixel, at most `max_side` pixels a
/// side.
pub fn bake_with(map: &[u8], files: &HashMap<u32, Vec<u8>>, scale: f32, max_side: usize) -> Result<WorldRender> {
    let file = Ffna::parse(map)?;
    let chunk = |id: u32| file.chunk(id).ok_or(RenderError::MapFile(MapFileError::MissingChunk(id)));
    let refs = |id: u32| file.chunk(id).map_or(Ok(Vec::new()), parse_file_refs);
    let params = MapParams::parse(chunk(0x1000_000C)?)?;
    let (terrain, surface) = TerrainStrip::parse_with_surface(chunk(0x1000_0002)?)?;
    let environment = file.chunk(0x1000_0009).and_then(|c| Environment::parse(c).ok());
    let props = file.chunk(0x1000_0004).and_then(|c| PropsStrip::parse(c).ok()).map_or(Vec::new(), |p| p.props);
    let model_refs = refs(0x1100_0004)?;

    let texture = |id: u32| files.get(&id).and_then(|data| atex::decode(data).ok()).map(Mipmapped::new);
    let terrain_textures: Vec<Option<Mipmapped>> = {
        let mut decoded: HashMap<u32, Option<Mipmapped>> = HashMap::new();
        refs(0x1100_0002)?.into_iter().map(|id| decoded.entry(id).or_insert_with(|| texture(id)).clone()).collect()
    };

    let water = environment.as_ref().and_then(|e| e.water()).filter(|_| params.has_water()).map(|w| WaterStyle {
        elevation: -w.surface_z,
        color: [w.absorption[0], w.absorption[1], w.absorption[2]].map(|c| c as f32 / 255.0 * 0.75),
    });

    let bounds = [params.min_x, params.min_y, params.max_x, params.max_y];
    let mut canvas = Canvas::new(View::fit(bounds, scale, max_side));
    terrain::draw(&mut canvas, &terrain, &surface, &terrain_textures, water);

    let models: HashMap<u32, Model> =
        dedup(model_refs.clone()).into_iter().filter_map(|id| Some((id, model::parse(files.get(&id)?)?))).collect();
    let model_textures: HashMap<u32, Mipmapped> = dedup(models.values().flat_map(|m| m.textures.clone()).collect())
        .into_iter()
        .filter_map(|id| Some((id, texture(id)?)))
        .collect();
    let sprites = props::rasterize(&canvas, &props, &model_refs, &models, &model_textures);
    let prop_models = props.iter().map(|p| model_refs.get(p.model as usize).copied().unwrap_or(0)).collect();
    Ok(WorldRender::new(&canvas, prop_models, &sprites))
}

/// A baked render: the terrain image and a sprite per drawn prop.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldRender {
    /// `[min_x, min_y, max_x, max_y]` of the whole image.
    pub bounds: [f32; 4],
    pub width: usize,
    pub height: usize,
    /// Terrain and water, RGB8, row-major from the top (`max_y`).
    pub terrain: Vec<u8>,
    /// Every placed prop, in props-chunk order.
    pub props: Vec<PropEntry>,
    /// The props that drew something, in prop order.
    pub sprites: Vec<SpriteImage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropEntry {
    /// Base file id of the prop's model; 0 when the map doesn't name one.
    pub model: u32,
    /// Index into [`WorldRender::sprites`]; `None` if the prop drew
    /// nothing (its model is missing, unsupported or under the terrain).
    pub sprite: Option<usize>,
}

/// A prop's pixels in a [`WorldRender`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpriteImage {
    /// Index into [`WorldRender::props`].
    pub prop: usize,
    /// x, y, width and height in image pixels.
    pub rect: [usize; 4],
    /// RGB8, row-major.
    pub rgb: Vec<u8>,
    /// Elevation rank per pixel: 0 where the prop isn't drawn, higher is
    /// above. Ranks compare across the sprites of one render.
    pub depth: Vec<u16>,
}

/// File signature: `GWRI`, then a version.
const MAGIC: &[u8; 4] = b"GWRI";
const FILE_VERSION: u32 = 2;
/// Sprites start on multiples of this in the atlas, so JPEG blocks (16 px
/// with chroma subsampling) never mix two sprites.
const ALIGN: usize = 16;
/// Narrowest atlas.
const ATLAS_WIDTH: usize = 4096;
/// Largest atlas side: the largest JPEG side, rounded down to [`ALIGN`].
const MAX_ATLAS_SIDE: usize = 65535 / ALIGN * ALIGN;

impl WorldRender {
    /// A render from the terrain in `canvas`, the model of each prop, and
    /// the props' sprites (in prop order).
    pub fn new(canvas: &Canvas, prop_models: Vec<u32>, sprites: &[props::Sprite]) -> Self {
        let v = canvas.view;
        // Ranks in steps of a world unit (finer would only add noise for
        // the depth image to store), or coarser if the range needs it.
        let finite = || sprites.iter().flat_map(|s| &s.elevation).copied().filter(|e| e.is_finite());
        let low = finite().fold(f32::MAX, f32::min);
        let step = ((finite().fold(f32::MIN, f32::max) - low) / 65534.0).max(DEPTH_STEP);
        let rank = |e: f32| if e.is_finite() { 1 + ((e - low) / step).round().min(65534.0) as u16 } else { 0 };

        let mut props: Vec<PropEntry> = prop_models.into_iter().map(|model| PropEntry { model, sprite: None }).collect();
        let sprites = sprites
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if let Some(p) = props.get_mut(s.prop) {
                    p.sprite = Some(i);
                }
                SpriteImage {
                    prop: s.prop,
                    rect: s.rect,
                    rgb: s.color.iter().flat_map(|&c| rgb8(c)).collect(),
                    depth: s.elevation.iter().map(|&e| rank(e)).collect(),
                }
            })
            .collect();
        Self { bounds: v.bounds(), width: v.width, height: v.height, terrain: canvas.to_rgb(), props, sprites }
    }

    /// RGB8 of the pixel rectangle `[x, y, width, height]`: the terrain,
    /// with the sprites of the props for which `visible(prop)` holds put
    /// over it by elevation.
    pub fn compose(&self, visible: impl Fn(usize) -> bool, [x0, y0, w, h]: [usize; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 3);
        for y in y0..y0 + h {
            let row = y * self.width + x0;
            out.extend_from_slice(&self.terrain[row * 3..(row + w) * 3]);
        }
        let mut depth = vec![0u16; w * h];
        for s in self.sprites.iter().filter(|s| visible(s.prop)) {
            let [sx, sy, sw, sh] = s.rect;
            for y in sy.max(y0)..(sy + sh).min(y0 + h) {
                for x in sx.max(x0)..(sx + sw).min(x0 + w) {
                    let (si, oi) = ((y - sy) * sw + x - sx, (y - y0) * w + x - x0);
                    if s.depth[si] > depth[oi] {
                        depth[oi] = s.depth[si];
                        out[oi * 3..oi * 3 + 3].copy_from_slice(&s.rgb[si * 3..si * 3 + 3]);
                    }
                }
            }
        }
        out
    }

    /// RGB8 of the whole image with every prop.
    pub fn compose_all(&self) -> Vec<u8> {
        self.compose(|_| true, [0, 0, self.width, self.height])
    }

    /// The stored form:
    /// - `GWRI`, `u32` version, 4 `f32` bounds, `u32` width and height
    /// - `u32` prop count, then per prop `u32` model and `u32` sprite
    ///   (`u32::MAX` for none)
    /// - `u32` sprite count, then per sprite `u32` prop, 4 `u32` rect and 2
    ///   `u32` atlas position
    /// - `u32` atlas width and height
    /// - length-prefixed: the terrain JPEG, the atlas colour JPEG and the
    ///   atlas depth (16-bit grey PNG); the atlas images are empty without
    ///   sprites
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, v: usize| out.extend_from_slice(&(v as u32).to_le_bytes());
        out.extend_from_slice(MAGIC);
        put(&mut out, FILE_VERSION as usize);
        for b in self.bounds {
            out.extend_from_slice(&b.to_le_bytes());
        }
        put(&mut out, self.width);
        put(&mut out, self.height);
        put(&mut out, self.props.len());
        for p in &self.props {
            put(&mut out, p.model as usize);
            put(&mut out, p.sprite.unwrap_or(u32::MAX as usize));
        }

        let sizes: Vec<[usize; 2]> = self.sprites.iter().map(|s| [s.rect[2], s.rect[3]]).collect();
        let ([aw, ah], positions) = pack(&sizes)?;
        put(&mut out, self.sprites.len());
        for (s, pos) in self.sprites.iter().zip(&positions) {
            put(&mut out, s.prop);
            s.rect.iter().chain(pos).for_each(|&v| put(&mut out, v));
        }
        put(&mut out, aw);
        put(&mut out, ah);

        let mut blob = |data: Vec<u8>| {
            put(&mut out, data.len());
            out.extend_from_slice(&data);
        };
        blob(jpeg(&self.terrain, self.width, self.height)?);
        if self.sprites.is_empty() {
            blob(Vec::new());
            blob(Vec::new());
            return Ok(out);
        }
        // Each sprite fills its aligned cell, its edge pixels repeated into
        // the padding so JPEG blocks see no hard edge.
        let mut rgb = vec![0u8; aw * ah * 3];
        let mut depth = vec![0u16; aw * ah];
        for (s, &[ax, ay]) in self.sprites.iter().zip(&positions) {
            let [.., sw, sh] = s.rect;
            for y in 0..align(sh) {
                for x in 0..align(sw) {
                    let (i, si) = ((ay + y) * aw + ax + x, y.min(sh - 1) * sw + x.min(sw - 1));
                    rgb[i * 3..i * 3 + 3].copy_from_slice(&s.rgb[si * 3..si * 3 + 3]);
                    if x < sw && y < sh {
                        depth[i] = s.depth[si];
                    }
                }
            }
        }
        blob(jpeg(&rgb, aw, ah)?);
        let depth: Vec<u8> = depth.iter().flat_map(|d| d.to_ne_bytes()).collect();
        let mut png = Vec::new();
        // Elevation is linear across a triangle, which the Paeth and Up
        // filters predict well.
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        image::ImageEncoder::write_image(
            PngEncoder::new_with_quality(&mut png, CompressionType::Best, FilterType::Adaptive),
            &depth,
            aw as u32,
            ah as u32,
            image::ExtendedColorType::L16,
        )
        .map_err(|e| RenderError::Image(e.to_string()))?;
        blob(png);
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut r = Cursor::header(data)?;
        let bounds = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
        let (width, height) = (r.usize()?, r.usize()?);
        let props = (0..r.usize()?)
            .map(|_| {
                let model = r.u32()?;
                let sprite = r.u32()?;
                Ok(PropEntry { model, sprite: (sprite != u32::MAX).then_some(sprite as usize) })
            })
            .collect::<Result<Vec<_>>>()?;
        let refs = (0..r.usize()?)
            .map(|_| Ok((r.usize()?, [r.usize()?, r.usize()?, r.usize()?, r.usize()?], [r.usize()?, r.usize()?])))
            .collect::<Result<Vec<_>>>()?;
        let (aw, _ah) = (r.usize()?, r.usize()?);

        let terrain = decode_image(r.blob()?, image::ImageFormat::Jpeg)?.into_rgb8();
        if (terrain.width() as usize, terrain.height() as usize) != (width, height) {
            return Err(RenderError::Image("terrain image size mismatch".into()));
        }
        let (atlas_rgb, atlas_depth) = if refs.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            (
                decode_image(r.blob()?, image::ImageFormat::Jpeg)?.into_rgb8().into_raw(),
                decode_image(r.blob()?, image::ImageFormat::Png)?.into_luma16().into_raw(),
            )
        };
        let sprites = refs
            .into_iter()
            .map(|(prop, rect, [ax, ay])| {
                let [x, y, w, h] = rect;
                if x + w > width || y + h > height || (ax + w) > aw || (ay + h) * aw > atlas_depth.len() {
                    return Err(RenderError::Image("sprite out of bounds".into()));
                }
                let (mut rgb, mut depth) = (Vec::with_capacity(w * h * 3), Vec::with_capacity(w * h));
                for row in ay..ay + h {
                    let i = row * aw + ax;
                    rgb.extend_from_slice(&atlas_rgb[i * 3..(i + w) * 3]);
                    depth.extend_from_slice(&atlas_depth[i..i + w]);
                }
                Ok(SpriteImage { prop, rect, rgb, depth })
            })
            .collect::<Result<_>>()?;
        Ok(Self { bounds, width, height, terrain: terrain.into_raw(), props, sprites })
    }
}

fn align(v: usize) -> usize {
    v.div_ceil(ALIGN) * ALIGN
}

/// Shelf-pack rectangles of `sizes` (width, height), each padded to
/// [`ALIGN`]: the atlas size and each one's position. The atlas is
/// [`ATLAS_WIDTH`] wide, or wider if it would be too tall.
fn pack(sizes: &[[usize; 2]]) -> Result<([usize; 2], Vec<[usize; 2]>)> {
    if sizes.is_empty() {
        return Ok(([0, 0], Vec::new()));
    }
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sizes[i][1]));
    let widest = sizes.iter().map(|s| align(s[0])).max().unwrap_or(0);
    let mut width = ATLAS_WIDTH.max(widest);
    loop {
        let mut positions = vec![[0, 0]; sizes.len()];
        let (mut x, mut y, mut shelf) = (0, 0, 0);
        for &i in &order {
            let [w, h] = sizes[i].map(align);
            if x + w > width {
                (x, y, shelf) = (0, y + shelf, 0);
            }
            positions[i] = [x, y];
            x += w;
            shelf = shelf.max(h);
        }
        let height = y + shelf;
        if height <= MAX_ATLAS_SIDE && width <= MAX_ATLAS_SIDE {
            return Ok(([width, height], positions));
        }
        if width >= MAX_ATLAS_SIDE {
            return Err(RenderError::Image(format!("prop sprites don't fit an atlas ({width}x{height})")));
        }
        width = (width * 2).min(MAX_ATLAS_SIDE);
    }
}

fn jpeg(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, QUALITY)
        .encode(rgb, width as u32, height as u32, image::ExtendedColorType::Rgb8)
        .map_err(|e| RenderError::Image(e.to_string()))?;
    Ok(out)
}

fn decode_image(data: &[u8], format: image::ImageFormat) -> Result<image::DynamicImage> {
    image::load_from_memory_with_format(data, format).map_err(|e| RenderError::Image(e.to_string()))
}

/// Reads the stored form's little-endian fields.
struct Cursor<'a> {
    data: &'a [u8],
}

impl<'a> Cursor<'a> {
    /// After the signature and version.
    fn header(data: &'a [u8]) -> Result<Self> {
        if data.len() < 8 || &data[..4] != MAGIC {
            return Err(RenderError::Image("not a world render".into()));
        }
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != FILE_VERSION {
            return Err(RenderError::Image(format!("unsupported world render version {version}")));
        }
        Ok(Self { data: &data[8..] })
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len() < n {
            return Err(RenderError::Image("truncated world render".into()));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn usize(&mut self) -> Result<usize> {
        self.u32().map(|v| v as usize)
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn blob(&mut self) -> Result<&'a [u8]> {
        let n = self.usize()?;
        self.bytes(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both reference maps render without any of their files (plain
    /// colours stand in for the textures, props are left out).
    #[test]
    fn bakes_without_files() {
        for pair in crate::mapfile::testdata::MapPair::all() {
            let Some(map) = pair.strip() else { continue };
            assert!(!required_files(&map).unwrap().is_empty());
            let render = bake_with(&map, &HashMap::new(), 96.0, 1024).unwrap();
            assert!(render.sprites.is_empty() && !render.props.is_empty());
            let rgb = render.compose_all();
            assert_eq!(rgb.len(), render.width * render.height * 3);
            let distinct = rgb.chunks_exact(3).collect::<HashSet<_>>().len();
            assert!(distinct > 16, "{pair:?}: {distinct} colours");
        }
    }

    /// A 40x30 render with two overlapping sprites: the first higher on the
    /// left, the second higher on the right.
    fn two_sprites() -> WorldRender {
        let (width, height) = (40, 30);
        let sprite = |prop: usize, x: usize, colour: u8, rising: bool| {
            let [w, h] = [20, 10];
            let depth = (0..w * h).map(|i| { let c = (i % w) as u16; 100 + if rising { c } else { 20 - c } }).collect();
            SpriteImage { prop, rect: [x, 10, w, h], rgb: vec![colour; w * h * 3], depth }
        };
        WorldRender {
            bounds: [0.0, 0.0, 40.0, 30.0],
            width,
            height,
            terrain: vec![40; width * height * 3],
            props: vec![
                PropEntry { model: 7, sprite: Some(0) },
                PropEntry { model: 0, sprite: None },
                PropEntry { model: 9, sprite: Some(1) },
            ],
            sprites: vec![sprite(0, 5, 200, false), sprite(2, 15, 120, true)],
        }
    }

    #[test]
    fn compose_by_elevation() {
        let render = two_sprites();
        let px = |rgb: &[u8], x: usize, y: usize| rgb[(y * 40 + x) * 3];
        let all = render.compose_all();
        // Overlap is x 15..25: sprite 0 falls from 120 to 101 over x 5..25,
        // sprite 1 rises from 100 over x 15..35.
        assert_eq!([px(&all, 2, 15), px(&all, 10, 15), px(&all, 16, 15), px(&all, 24, 15)], [40, 200, 200, 120]);
        let without_first = render.compose(|p| p != 0, [0, 0, 40, 30]);
        assert_eq!([px(&without_first, 10, 15), px(&without_first, 16, 15)], [40, 120]);
        // A sub-rectangle matches the same pixels of the whole.
        let part = render.compose(|_| true, [12, 12, 10, 5]);
        assert_eq!(part[..30], all[(12 * 40 + 12) * 3..(12 * 40 + 22) * 3]);
    }

    #[test]
    fn render_roundtrip() {
        let render = two_sprites();
        let back = WorldRender::decode(&render.encode().unwrap()).unwrap();
        assert_eq!((back.bounds, back.width, back.height), (render.bounds, 40, 30));
        assert_eq!(back.props, render.props);
        for (a, b) in back.sprites.iter().zip(&render.sprites) {
            assert_eq!((a.prop, a.rect, &a.depth), (b.prop, b.rect, &b.depth));
            assert!(a.rgb.iter().zip(&b.rgb).all(|(x, y)| x.abs_diff(*y) <= 3));
        }
        assert!(back.terrain.iter().all(|&v| v.abs_diff(40) <= 2));
    }

    #[test]
    fn packs_aligned_without_overlap() {
        let sizes: Vec<[usize; 2]> = (1..200).map(|i| [(i * 37) % 900 + 1, (i * 53) % 300 + 1]).collect();
        let ([w, h], positions) = pack(&sizes).unwrap();
        assert!(w >= ATLAS_WIDTH && h <= MAX_ATLAS_SIDE);
        let cells: Vec<[usize; 4]> =
            positions.iter().zip(&sizes).map(|(&[x, y], &[sw, sh])| [x, y, x + align(sw), y + align(sh)]).collect();
        for (i, a) in cells.iter().enumerate() {
            assert!(a[0] % ALIGN == 0 && a[1] % ALIGN == 0 && a[2] <= w && a[3] <= h);
            for b in &cells[i + 1..] {
                assert!(a[2] <= b[0] || b[2] <= a[0] || a[3] <= b[1] || b[3] <= a[1], "{a:?} overlaps {b:?}");
            }
        }
        // Too tall for one width: the atlas widens instead.
        let ([w, h], _) = pack(&vec![[4000, 4000]; 40]).unwrap();
        assert!(w > ATLAS_WIDTH && h <= MAX_ATLAS_SIDE);
    }
}
