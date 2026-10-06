//! wgpu rendering of the pathing planes, drawn inside the egui frame
//! through a paint callback.
//!
//! The geometry of a map is built and uploaded once; per frame only the view
//! uniform changes. Vertices are in world units; the vertex shader maps them
//! to the callback's viewport.

use std::ops::Range;

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use gw_nav::mapfile::navmesh::{NONE_U16, PathPlane, Trapezoid};
use wgpu::util::DeviceExt;

const SHADER: &str = r#"
struct View {
    center: vec2<f32>,
    scale: vec2<f32>,
};
@group(0) @binding(0) var<uniform> view: View;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
    var out: VertexOut;
    out.position = vec4<f32>((pos - view.center) * view.scale, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Floats per vertex: x, y, then premultiplied RGBA.
const VERTEX_FLOATS: usize = 6;

/// Map geometry on the CPU, grouped by plane.
#[derive(Default)]
pub struct Mesh {
    pub fills: Vec<f32>,
    /// Walls (trapezoid sides) and splits (top and bottom edges).
    pub lines: Vec<f32>,
    /// Vertex ranges per plane.
    pub fill_ranges: Vec<Range<u32>>,
    pub wall_ranges: Vec<Range<u32>>,
    pub split_ranges: Vec<Range<u32>>,
}

/// Colour of a plane: plane 0 (the ground) is blue-grey, prop planes get
/// distinct hues.
pub fn plane_color(plane: usize) -> egui::Color32 {
    if plane == 0 {
        return egui::Color32::from_rgb(90, 120, 165);
    }
    let hue = (plane as f32 * 0.618_034).fract();
    egui::ecolor::Hsva::new(hue, 0.65, 0.95, 1.0).into()
}

fn push(buf: &mut Vec<f32>, [x, y]: [f32; 2], color: [f32; 4]) {
    buf.extend_from_slice(&[x, y]);
    buf.extend_from_slice(&color);
}

fn premultiplied(c: egui::Color32, alpha: f32) -> [f32; 4] {
    let [r, g, b, _] = c.to_normalized_gamma_f32();
    [r * alpha, g * alpha, b * alpha, alpha]
}

/// The corners of a trapezoid: top left, top right, bottom right, bottom
/// left.
pub fn corners(t: &Trapezoid) -> [[f32; 2]; 4] {
    [
        [t.x_top_left, t.y_top],
        [t.x_top_right, t.y_top],
        [t.x_bottom_right, t.y_bottom],
        [t.x_bottom_left, t.y_bottom],
    ]
}

impl Mesh {
    pub fn build(planes: &[PathPlane]) -> Self {
        let mut mesh = Mesh::default();
        let portal = premultiplied(egui::Color32::from_rgb(255, 210, 60), 1.0);
        let v = |n: usize| (n / VERTEX_FLOATS) as u32;
        for (i, plane) in planes.iter().enumerate() {
            let base = plane_color(i);
            let fill = premultiplied(base, if i == 0 { 0.55 } else { 0.7 });
            let wall = premultiplied(base.linear_multiply(0.35), 1.0);
            let split = premultiplied(base.linear_multiply(0.7), 0.5);
            let f0 = mesh.fills.len();
            for t in &plane.trapezoids {
                let [tl, tr, br, bl] = corners(t);
                for p in [tl, tr, br, tl, br, bl] {
                    push(&mut mesh.fills, p, fill);
                }
            }
            mesh.fill_ranges.push(v(f0)..v(mesh.fills.len()));

            let w0 = mesh.lines.len();
            for t in &plane.trapezoids {
                let [tl, tr, br, bl] = corners(t);
                for (a, b, portal_id) in [(tl, bl, t.portal_left), (tr, br, t.portal_right)] {
                    let color = if portal_id != NONE_U16 { portal } else { wall };
                    push(&mut mesh.lines, a, color);
                    push(&mut mesh.lines, b, color);
                }
            }
            let s0 = mesh.lines.len();
            for t in &plane.trapezoids {
                let [tl, tr, br, bl] = corners(t);
                for (a, b) in [(tl, tr), (bl, br)] {
                    push(&mut mesh.lines, a, split);
                    push(&mut mesh.lines, b, split);
                }
            }
            mesh.wall_ranges.push(v(w0)..v(s0));
            mesh.split_ranges.push(v(s0)..v(mesh.lines.len()));
        }
        mesh
    }
}

/// GPU state, stored in egui-wgpu's callback resources.
pub struct Gpu {
    fill_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    map: Option<GpuMap>,
}

struct GpuMap {
    generation: u64,
    fills: wgpu::Buffer,
    lines: wgpu::Buffer,
    fill_ranges: Vec<Range<u32>>,
    wall_ranges: Vec<Range<u32>>,
    split_ranges: Vec<Range<u32>>,
}

impl Gpu {
    /// Create the pipelines and register them with the egui renderer.
    pub fn install(render_state: &egui_wgpu::RenderState) {
        let device = &render_state.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pathing"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pathing view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pathing"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: (VERTEX_FLOATS * 4) as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        };
        let pipeline = |topology, label| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(vertex_layout.clone())],
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: render_state.target_format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let fill_pipeline = pipeline(wgpu::PrimitiveTopology::TriangleList, "pathing fills");
        let line_pipeline = pipeline(wgpu::PrimitiveTopology::LineList, "pathing lines");
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pathing view"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pathing view"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        render_state.renderer.write().callback_resources.insert(Gpu {
            fill_pipeline,
            line_pipeline,
            uniform,
            bind_group,
            map: None,
        });
    }

    /// Upload a map's geometry, replacing the previous one.
    pub fn upload(render_state: &egui_wgpu::RenderState, generation: u64, mesh: &Mesh) {
        let device = &render_state.device;
        let buffer = |data: &[f32], label| {
            // wgpu rejects empty buffers; a map without planes gets a dummy vertex.
            let data = if data.is_empty() { &[0.0; VERTEX_FLOATS][..] } else { data };
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(data),
                usage: wgpu::BufferUsages::VERTEX,
            })
        };
        let map = GpuMap {
            generation,
            fills: buffer(&mesh.fills, "pathing fills"),
            lines: buffer(&mesh.lines, "pathing lines"),
            fill_ranges: mesh.fill_ranges.clone(),
            wall_ranges: mesh.wall_ranges.clone(),
            split_ranges: mesh.split_ranges.clone(),
        };
        if let Some(gpu) = render_state.renderer.write().callback_resources.get_mut::<Gpu>() {
            gpu.map = Some(map);
        }
    }
}

/// One frame's draw of the map.
pub struct DrawMap {
    pub generation: u64,
    /// World point at the centre of the viewport.
    pub center: [f32; 2],
    /// World units to NDC.
    pub scale: [f32; 2],
    pub visible: Vec<bool>,
    pub fills: bool,
    pub walls: bool,
    pub splits: bool,
}

impl egui_wgpu::CallbackTrait for DrawMap {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(gpu) = resources.get::<Gpu>() {
            let view = [self.center[0], self.center[1], self.scale[0], self.scale[1]];
            queue.write_buffer(&gpu.uniform, 0, bytemuck::cast_slice(&view));
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(gpu) = resources.get::<Gpu>() else { return };
        let Some(map) = gpu.map.as_ref().filter(|m| m.generation == self.generation) else { return };
        pass.set_bind_group(0, &gpu.bind_group, &[]);
        let layers = [
            (self.fills, &gpu.fill_pipeline, &map.fills, &map.fill_ranges),
            (self.splits, &gpu.line_pipeline, &map.lines, &map.split_ranges),
            (self.walls, &gpu.line_pipeline, &map.lines, &map.wall_ranges),
        ];
        for (enabled, pipeline, buffer, ranges) in layers {
            if !enabled {
                continue;
            }
            pass.set_pipeline(pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            for (range, _) in ranges.iter().zip(&self.visible).filter(|(_, v)| **v) {
                if !range.is_empty() {
                    pass.draw(range.clone(), 0..1);
                }
            }
        }
    }
}
