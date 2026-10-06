//! The output image: a top-down orthographic view of the map bounds, with
//! an elevation buffer for depth testing props against the terrain.

/// World-to-pixel mapping of a top-down image. Pixel rows run from `max_y`
/// (top) down, columns from `min_x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub min_x: f32,
    pub max_y: f32,
    /// World units per pixel.
    pub scale: f32,
    pub width: usize,
    pub height: usize,
}

impl View {
    /// A view of `bounds` (`[min_x, min_y, max_x, max_y]`) at `scale` world
    /// units per pixel, enlarged as needed so neither side exceeds
    /// `max_side` pixels.
    pub fn fit(bounds: [f32; 4], scale: f32, max_side: usize) -> Self {
        let [min_x, min_y, max_x, max_y] = bounds;
        let (w, h) = (max_x - min_x, max_y - min_y);
        let scale = scale.max(w.max(h) / max_side as f32);
        Self {
            min_x,
            max_y,
            scale,
            width: ((w / scale).ceil() as usize).max(1),
            height: ((h / scale).ceil() as usize).max(1),
        }
    }

    /// World position of a pixel centre.
    pub fn world(&self, px: usize, py: usize) -> [f32; 2] {
        [self.min_x + (px as f32 + 0.5) * self.scale, self.max_y - (py as f32 + 0.5) * self.scale]
    }

    /// Pixel coordinates (continuous) of a world position.
    pub fn pixel(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [(x - self.min_x) / self.scale, (self.max_y - y) / self.scale]
    }

    /// The world rectangle the image covers: `[min_x, min_y, max_x, max_y]`.
    pub fn bounds(&self) -> [f32; 4] {
        let (w, h) = (self.width as f32 * self.scale, self.height as f32 * self.scale);
        [self.min_x, self.max_y - h, self.min_x + w, self.max_y]
    }
}

/// Colour and elevation per pixel. Elevation is up-positive (the negated
/// GW z).
#[derive(Debug, Clone)]
pub struct Canvas {
    pub view: View,
    pub color: Vec<[f32; 3]>,
    pub elevation: Vec<f32>,
}

impl Canvas {
    pub fn new(view: View) -> Self {
        let n = view.width * view.height;
        Self { view, color: vec![[0.0; 3]; n], elevation: vec![f32::NEG_INFINITY; n] }
    }

    pub fn to_rgba(&self) -> Vec<u8> {
        self.color.iter().flat_map(|&c| { let [r, g, b] = rgb8(c); [r, g, b, 255] }).collect()
    }

    pub fn to_rgb(&self) -> Vec<u8> {
        self.color.iter().flat_map(|&c| rgb8(c)).collect()
    }
}

/// A linear 0..1 colour as RGB8.
pub fn rgb8(c: [f32; 3]) -> [u8; 3] {
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
}

/// How many threads to render with.
pub fn threads() -> usize {
    // No threads on the web.
    if cfg!(target_arch = "wasm32") {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get()).min(16)
    }
}

/// Run `f(y0, y1, colors, elevations)` on bands of rows `y0..y1`, in
/// parallel; the slices hold exactly those rows.
pub fn par_bands(canvas: &mut Canvas, f: impl Fn(usize, usize, &mut [[f32; 3]], &mut [f32]) + Sync) {
    let (width, height) = (canvas.view.width, canvas.view.height);
    let threads = threads();
    let rows_per = height.div_ceil(threads).max(1);
    if threads == 1 {
        f(0, height, &mut canvas.color, &mut canvas.elevation);
        return;
    }
    std::thread::scope(|s| {
        for (band, (colors, elevations)) in
            canvas.color.chunks_mut(rows_per * width).zip(canvas.elevation.chunks_mut(rows_per * width)).enumerate()
        {
            let f = &f;
            let y0 = band * rows_per;
            let y1 = y0 + colors.len() / width;
            s.spawn(move || f(y0, y1, colors, elevations));
        }
    });
}

/// Run `f(row, colors, elevations)` for each pixel row, in parallel.
pub fn par_rows(canvas: &mut Canvas, f: impl Fn(usize, &mut [[f32; 3]], &mut [f32]) + Sync) {
    let width = canvas.view.width;
    par_bands(canvas, |y0, _, colors, elevations| {
        for (i, (c, e)) in colors.chunks_mut(width).zip(elevations.chunks_mut(width)).enumerate() {
            f(y0 + i, c, e);
        }
    });
}
