//! The visualizer window: the MapDb table, the map view, its layers
//! (including mission points and zone exits) and the waypointer.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::egui_wgpu;
use egui_extras::{Column, TableBuilder};
use gw_nav::PathingData;
use gw_nav::api::{MapAnnotations, MapEntry};
use gw_nav::mapfile::navmesh::{NONE_U16, Trapezoid};
use gw_nav::pathfind::NavMesh;
use gw_nav::render::WorldRender;

use crate::annotations::{self, Layers};
use crate::source::{Event, Source};
use crate::render::{DrawMap, Gpu, Mesh, corners, plane_color};
use crate::waypoints::Waypointer;

/// Where to get data from.
pub enum SourceKind {
    #[cfg(not(target_arch = "wasm32"))]
    Local { db: std::path::PathBuf, cache_dir: std::path::PathBuf, zones_db: Option<std::path::PathBuf> },
    Relay(String),
}

pub struct Options {
    pub source: SourceKind,
    pub initial_mapfile: Option<u32>,
}

/// World-to-screen transform: `zoom` screen points per world unit around
/// `center`. World y points up, screen y down.
#[derive(Clone, Copy)]
pub struct View {
    pub center: [f32; 2],
    pub zoom: f32,
}

impl View {
    pub fn to_screen(self, rect: Rect, [x, y]: [f32; 2]) -> Pos2 {
        rect.center() + Vec2::new((x - self.center[0]) * self.zoom, (self.center[1] - y) * self.zoom)
    }

    pub fn to_world(self, rect: Rect, p: Pos2) -> [f32; 2] {
        let d = p - rect.center();
        [self.center[0] + d.x / self.zoom, self.center[1] - d.y / self.zoom]
    }
}

struct LoadedMap {
    data: PathingData,
    /// For paths between waypoints.
    nav: NavMesh,
    /// Arrives shortly after the map.
    annotations: Option<MapAnnotations>,
    /// The baked top-down render; arrives after the annotations.
    background: Option<Background>,
    bounds: [f32; 4],
    generation: u64,
    /// The status line for the loaded map.
    summary: String,
}

/// A map render as GPU textures: tiles small enough for any backend, each
/// composited from the terrain and the visible props.
struct Background {
    render: WorldRender,
    tiles: Vec<Tile>,
    /// Whether each prop is drawn, by index in the props chunk.
    prop_visible: Vec<bool>,
    /// The path planes each prop owns.
    prop_planes: Vec<Vec<usize>>,
    /// The drawn props that own no path plane.
    planeless: Vec<usize>,
    /// The layer of [`Self::planeless`]; off hides them whatever
    /// `prop_visible` says.
    show_planeless: bool,
    /// Shows only the props whose index or model contains it.
    prop_filter: String,
}

struct Tile {
    /// Pixel rectangle in the render: x, y, width, height.
    pixels: [usize; 4],
    texture: egui::TextureHandle,
}

impl Background {
    /// Largest tile side; WebGL guarantees 2048.
    const TILE: usize = 2048;

    fn new(ctx: &egui::Context, render: WorldRender, plane_props: &[u16], show_planeless: bool) -> Self {
        let prop_visible = vec![true; render.props.len()];
        let mut prop_planes = vec![Vec::new(); render.props.len()];
        // Plane 0 is the ground.
        for (plane, &prop) in plane_props.iter().enumerate().skip(1) {
            if let Some(planes) = prop_planes.get_mut(prop as usize) {
                planes.push(plane);
            }
        }
        let planeless = (0..render.props.len())
            .filter(|&p| render.props[p].sprite.is_some() && prop_planes[p].is_empty())
            .collect();
        let mut background = Self {
            render,
            tiles: Vec::new(),
            prop_visible,
            prop_planes,
            planeless,
            show_planeless,
            prop_filter: String::new(),
        };
        let (width, height) = (background.render.width, background.render.height);
        for y in (0..height).step_by(Self::TILE) {
            for x in (0..width).step_by(Self::TILE) {
                let pixels = [x, y, Self::TILE.min(width - x), Self::TILE.min(height - y)];
                let image = background.composite(pixels);
                let texture = ctx.load_texture(format!("map render {x},{y}"), image, egui::TextureOptions::LINEAR);
                background.tiles.push(Tile { pixels, texture });
            }
        }
        background
    }

    fn is_visible(&self, prop: usize) -> bool {
        let shown = self.prop_visible.get(prop).copied().unwrap_or(true);
        shown && (self.show_planeless || self.prop_planes.get(prop).is_some_and(|planes| !planes.is_empty()))
    }

    fn composite(&self, [x, y, w, h]: [usize; 4]) -> egui::ColorImage {
        let rgb = self.render.compose(|p| self.is_visible(p), [x, y, w, h]);
        egui::ColorImage::from_rgb([w, h], &rgb)
    }

    /// Show or hide the props that own no path plane.
    fn set_show_planeless(&mut self, on: bool) {
        if self.show_planeless != on {
            self.show_planeless = on;
            self.update(&self.planeless.clone());
        }
    }

    /// Composite again the tiles that the sprites of `changed` props touch.
    fn update(&mut self, changed: &[usize]) {
        let rects: Vec<[usize; 4]> = changed.iter().filter_map(|&p| self.sprite_pixels(p)).collect();
        let stale: Vec<usize> = (0..self.tiles.len())
            .filter(|&i| {
                let [tx, ty, tw, th] = self.tiles[i].pixels;
                rects.iter().any(|&[x, y, w, h]| x < tx + tw && tx < x + w && y < ty + th && ty < y + h)
            })
            .collect();
        for i in stale {
            let image = self.composite(self.tiles[i].pixels);
            self.tiles[i].texture.set(image, egui::TextureOptions::LINEAR);
        }
    }

    fn sprite_pixels(&self, prop: usize) -> Option<[usize; 4]> {
        Some(self.render.sprites[self.render.props.get(prop)?.sprite?].rect)
    }

    /// The world rectangle (`[min_x, min_y, max_x, max_y]`) of pixels
    /// `[x, y, width, height]`.
    fn world_rect(&self, [x, y, w, h]: [usize; 4]) -> [f32; 4] {
        let [x0, _, x1, y1] = self.render.bounds;
        let scale = (x1 - x0) / self.render.width as f32;
        let at = |px: usize, py: usize| [x0 + px as f32 * scale, y1 - py as f32 * scale];
        let ([left, top], [right, bottom]) = (at(x, y), at(x + w, y + h));
        [left, bottom, right, top]
    }

    fn paint(&self, painter: &egui::Painter, rect: Rect, view: View, opacity: f32) {
        let tint = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0) as u8);
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        for tile in &self.tiles {
            let screen = screen_rect(rect, view, self.world_rect(tile.pixels));
            if screen.intersects(rect) {
                painter.image(tile.texture.id(), screen, uv, tint);
            }
        }
    }

    /// The Props list: a checkbox per prop, with all/none for the listed
    /// ones. Returns the prop under the pointer.
    fn props_panel(&mut self, ui: &mut egui::Ui) -> Option<usize> {
        let filter = self.prop_filter.trim().to_owned();
        let listed: Vec<usize> = (0..self.render.props.len())
            .filter(|&i| filter.is_empty() || i.to_string().contains(&filter) || self.render.props[i].model.to_string().contains(&filter))
            .collect();
        let mut changed = Vec::new();
        let mut hovered = None;
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.prop_filter).desired_width(80.0))
                .on_hover_text("Prop index or model file id");
            for (label, on) in [("all", true), ("none", false)] {
                if ui.small_button(label).on_hover_text("Applies to the listed props").clicked() {
                    for &i in &listed {
                        if self.prop_visible[i] != on && self.render.props[i].sprite.is_some() {
                            self.prop_visible[i] = on;
                            changed.push(i);
                        }
                    }
                }
            }
        });
        let row_height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().id_salt("props").auto_shrink([false, true]).show_rows(
            ui,
            row_height,
            listed.len(),
            |ui, range| {
                for &i in &listed[range] {
                    let entry = self.render.props[i];
                    let mut label = format!("#{i} model {}", entry.model);
                    if !self.prop_planes[i].is_empty() {
                        let planes: Vec<String> = self.prop_planes[i].iter().map(|p| p.to_string()).collect();
                        label += &format!(" · planes {}", planes.join(","));
                    }
                    if entry.sprite.is_none() {
                        label += " (not drawn)";
                    }
                    let response = ui.add_enabled(
                        entry.sprite.is_some(),
                        egui::Checkbox::new(&mut self.prop_visible[i], label),
                    );
                    if response.changed() {
                        changed.push(i);
                    }
                    if response.hovered() {
                        hovered = Some(i);
                    }
                }
            },
        );
        if !changed.is_empty() {
            self.update(&changed);
        }
        hovered
    }
}

/// The screen rectangle of a world rectangle (`[min_x, min_y, max_x, max_y]`).
fn screen_rect(rect: Rect, view: View, [x0, y0, x1, y1]: [f32; 4]) -> Rect {
    Rect::from_two_pos(view.to_screen(rect, [x0, y1]), view.to_screen(rect, [x1, y0]))
}

/// Display options that persist across runs.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    show_fills: bool,
    show_walls: bool,
    show_splits: bool,
    show_start_points: bool,
    show_hover_outline: bool,
    /// The baked map render under the pathing layers.
    show_background: bool,
    background_opacity: f32,
    /// The props in the map render that own no path plane.
    show_planeless_props: bool,
    layers: Layers,
    waypoint_window: bool,
    /// The side panels; hidden ones are reopened from the status bar.
    show_maps_panel: bool,
    show_layers_panel: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_fills: true,
            show_walls: true,
            show_splits: false,
            show_start_points: true,
            show_hover_outline: true,
            show_background: true,
            background_opacity: 1.0,
            show_planeless_props: true,
            layers: Layers { points: true, portals: true, exits: true },
            waypoint_window: true,
            show_maps_panel: true,
            show_layers_panel: false,
        }
    }
}

pub struct App {
    zones: Vec<MapEntry>,
    maps_error: Option<String>,
    filter: String,
    manual_id: String,
    source: Source,
    status: String,
    progress: Option<f32>,
    /// The row picked in the Maps list: its mapid and mapfile.
    selected: Option<(Option<u32>, Option<u32>)>,
    map: Option<LoadedMap>,
    generation: u64,
    gpu: Option<egui_wgpu::RenderState>,
    view: View,
    fit_pending: bool,
    visible: Vec<bool>,
    settings: Settings,
    waypointer: Waypointer,
    /// Where the map view was last drawn.
    map_rect: Rect,
    /// The prop under the pointer in the Props list, outlined on the map.
    hovered_prop: Option<usize>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, options: Options) -> Self {
        let gpu = cc.wgpu_render_state.clone();
        if let Some(rs) = &gpu {
            Gpu::install(rs);
        }
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut waypointer = Waypointer::new();
        waypointer.open = settings.waypoint_window;
        let ctx = cc.egui_ctx.clone();
        let mut source = match options.source {
            #[cfg(not(target_arch = "wasm32"))]
            SourceKind::Local { db, cache_dir, zones_db } => Source::local(db, cache_dir, zones_db, ctx),
            SourceKind::Relay(base) => Source::relay(base, ctx),
        };
        source.request_maps();
        if let Some(id) = options.initial_mapfile {
            source.load(id, false);
        }
        Self {
            zones: Vec::new(),
            maps_error: None,
            filter: String::new(),
            manual_id: options.initial_mapfile.map(|id| id.to_string()).unwrap_or_default(),
            status: format!("Select a map (data: {})", source.describe()),
            source,
            progress: None,
            selected: None,
            map: None,
            generation: 0,
            gpu,
            view: View { center: [0.0, 0.0], zoom: 0.01 },
            fit_pending: false,
            visible: Vec::new(),
            settings,
            waypointer,
            map_rect: Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0)),
            hovered_prop: None,
        }
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        for event in self.source.poll() {
            match event {
                Event::Progress(text, fraction) => {
                    self.status = text;
                    self.progress = fraction;
                }
                Event::Loaded(data) => {
                    self.progress = None;
                    self.status = format!(
                        "Map file {} (revision {}): {} planes, {} trapezoids",
                        data.mapfile_id,
                        data.file_id,
                        data.planes.len(),
                        data.trapezoid_count()
                    );
                    self.set_map(*data, self.status.clone());
                }
                Event::Failed(id, error) => {
                    self.progress = None;
                    self.status = format!("Loading map file {id} failed: {error}");
                }
                Event::Maps(Ok(zones)) => {
                    self.zones = zones;
                    self.maps_error = None;
                }
                Event::Maps(Err(e)) => self.maps_error = Some(e),
                Event::Annotations(id, a) => {
                    if let Some(map) = self.map.as_mut().filter(|m| m.data.mapfile_id == id) {
                        map.annotations = Some(*a);
                    }
                }
                Event::Background(id, image) => {
                    if let Some(map) = self.map.as_mut().filter(|m| m.data.mapfile_id == id) {
                        self.progress = None;
                        match image {
                            Ok(render) => {
                                map.background = Some(Background::new(
                                    ctx,
                                    *render,
                                    &map.data.plane_props,
                                    self.settings.show_planeless_props,
                                ));
                                self.status = map.summary.clone();
                            }
                            Err(e) => self.status = format!("{}; no map render: {e}", map.summary),
                        }
                    }
                }
            }
        }
    }

    fn set_map(&mut self, data: PathingData, summary: String) {
        self.generation += 1;
        if let Some(rs) = &self.gpu {
            Gpu::upload(rs, self.generation, &Mesh::build(&data.planes));
        }
        self.visible = vec![true; data.planes.len()];
        let nav = NavMesh::new(&data.planes);
        self.map = Some(LoadedMap {
            bounds: bounds(&data),
            nav,
            annotations: None,
            background: None,
            data,
            generation: self.generation,
            summary,
        });
        self.fit_pending = true;
    }

    /// The map id of the loaded map file: the one picked in the Maps list,
    /// or else the only map using that file.
    fn loaded_mapid(&self) -> Option<u32> {
        let file = Some(self.map.as_ref()?.data.mapfile_id);
        let mut maps = self.zones.iter().filter(|z| z.mapfile == file).filter_map(|z| z.mapid);
        match self.selected {
            Some((Some(id), selected_file)) if selected_file == file => Some(id),
            _ => maps.next().filter(|_| maps.next().is_none()),
        }
    }

    fn maps_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Maps");
            if ui.small_button("reload").clicked() {
                self.source.request_maps();
            }
            if self.source.can_import()
                && ui
                    .small_button("import…")
                    .on_hover_text("Merge map rows from another MapDb file. Rows with a mapfile replace ours.")
                    .clicked()
                && let Some((file, result)) = self.source.import_maps()
            {
                self.status = match result {
                    Ok(summary) => format!("Imported {file}: {summary}"),
                    Err(e) => format!("Importing {file} failed: {e}"),
                };
                self.source.request_maps();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("◀").on_hover_text("Hide the Maps panel").clicked() {
                    self.settings.show_maps_panel = false;
                }
            });
        });
        if let Some(e) = &self.maps_error {
            ui.colored_label(Color32::LIGHT_RED, format!("Map list unavailable: {e}"));
        }
        ui.horizontal(|ui| {
            ui.label("Map file id");
            ui.add(egui::TextEdit::singleline(&mut self.manual_id).desired_width(80.0));
            let id = self.manual_id.trim().parse::<u32>().ok();
            let idle = self.source.busy.is_none();
            if ui.add_enabled(idle && id.is_some(), egui::Button::new("Load")).clicked() {
                self.source.load(id.unwrap(), false);
            }
            if ui
                .add_enabled(idle && id.is_some(), egui::Button::new("Regenerate"))
                .on_hover_text("Download the current revision and generate again, ignoring the cache")
                .clicked()
            {
                self.source.load(id.unwrap(), true);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.filter);
        });
        ui.separator();

        let filter = self.filter.to_lowercase();
        let rows: Vec<&MapEntry> = self
            .zones
            .iter()
            .filter(|z| {
                filter.is_empty()
                    || z.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&filter))
                    || z.mapid.is_some_and(|id| id.to_string().contains(&filter))
                    || z.mapfile.is_some_and(|f| f.to_string().contains(&filter))
            })
            .collect();
        let mut clicked = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .column(Column::auto().at_least(40.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::remainder())
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("mapid");
                });
                header.col(|ui| {
                    ui.strong("mapfile");
                });
                header.col(|ui| {
                    ui.strong("name");
                });
            })
            .body(|body| {
                body.rows(18.0, rows.len(), |mut row| {
                    let zone = rows[row.index()];
                    row.set_selected(self.selected == Some((zone.mapid, zone.mapfile)));
                    row.col(|ui| {
                        ui.label(zone.mapid.map_or("-".into(), |id| id.to_string()));
                    });
                    row.col(|ui| {
                        ui.label(zone.mapfile.map_or("-".into(), |f| f.to_string()));
                    });
                    row.col(|ui| match (&zone.name, zone.mapid) {
                        (Some(name), _) => {
                            ui.label(name);
                        }
                        (None, Some(_)) => {
                            ui.label("-");
                        }
                        (None, None) => {
                            ui.weak("unidentified (from manifest)").on_hover_text(
                                "A map file in the fileserver's asset manifest that no MapDb row names yet.                                  Its map id is learned when GWBS logs the map being loaded.",
                            );
                        }
                    });
                    if row.response().clicked() {
                        clicked = Some((zone.mapid, zone.mapfile));
                    }
                });
            });
        if let Some((mapid, mapfile)) = clicked {
            self.selected = Some((mapid, mapfile));
            match mapfile {
                Some(id) if self.source.busy.is_none() => {
                    self.manual_id = id.to_string();
                    self.source.load(id, false);
                }
                Some(_) => {}
                None => self.status = format!("Map {} has no map file id", mapid.unwrap_or(0)),
            }
        }
    }

    fn layers_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.small_button("▶").on_hover_text("Hide the Layers panel").clicked() {
                self.settings.show_layers_panel = false;
            }
            ui.heading("Layers");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.settings.show_background, "World render")
                .on_hover_text(
                    "The map's terrain and props, rendered top-down when its pathing data was generated. \
                     Props can be hidden one by one in the Props list.",
                );
            ui.add_enabled(
                self.settings.show_background,
                egui::Slider::new(&mut self.settings.background_opacity, 0.0..=1.0).show_value(false),
            )
            .on_hover_text("Opacity");
        });
        let background = self.map.as_mut().and_then(|m| m.background.as_mut());
        let planeless = background.as_ref().map_or(String::new(), |b| format!(" ({})", b.planeless.len()));
        ui.indent("world render layers", |ui| {
            ui.add_enabled(
                self.settings.show_background,
                egui::Checkbox::new(&mut self.settings.show_planeless_props, format!("Props without planes{planeless}")),
            )
            .on_hover_text(
                "The props that own no path plane: trees, rocks, fences and the like. Hiding them leaves the \
                 terrain and the props you can walk on. Props hidden in the Props list stay hidden.",
            );
        });
        if let Some(background) = background {
            background.set_show_planeless(self.settings.show_planeless_props);
        }
        ui.checkbox(&mut self.settings.show_fills, "Trapezoid fills");
        ui.checkbox(&mut self.settings.show_walls, "Walls and portals")
            .on_hover_text("Trapezoid sides: the plane's boundary segments. Portal edges are yellow.");
        ui.checkbox(&mut self.settings.show_splits, "Trapezoid splits")
            .on_hover_text("Top and bottom trapezoid edges (internal horizontal splits)");
        ui.checkbox(&mut self.settings.show_start_points, "Start points");
        ui.checkbox(&mut self.settings.show_hover_outline, "Hover outline")
            .on_hover_text("Outline the trapezoid under the cursor");
        annotations::panel(ui, self.map.as_ref().and_then(|m| m.annotations.as_ref()), &mut self.settings.layers);
        ui.checkbox(&mut self.waypointer.open, "Waypoint window");
        if ui.button("Fit to map").clicked() {
            self.fit_pending = true;
        }
        self.hovered_prop = None;
        let Some(map) = &mut self.map else { return };
        ui.separator();
        // The two lists share the space left.
        let half = (ui.available_height() / 2.0 - 40.0).max(120.0);
        egui::CollapsingHeader::new(format!("Planes ({})", map.data.planes.len())).default_open(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("all").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = true);
                }
                if ui.small_button("none").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = false);
                }
            });
            egui::ScrollArea::vertical().id_salt("planes").max_height(half).show(ui, |ui| {
                for (i, plane) in map.data.planes.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, plane_color(i));
                        let label = if i == 0 {
                            format!("0 ground ({} trapezoids)", plane.trapezoids.len())
                        } else {
                            let prop = map.data.plane_props.get(i).copied().unwrap_or(0);
                            format!("{i} prop {prop} ({} trapezoids)", plane.trapezoids.len())
                        };
                        ui.checkbox(&mut self.visible[i], label);
                    });
                }
            });
        });
        let Some(background) = &mut map.background else { return };
        egui::CollapsingHeader::new(format!("Props ({})", background.render.props.len()))
            .default_open(true)
            .show(ui, |ui| self.hovered_prop = background.props_panel(ui))
            .header_response
            .on_hover_text("The props drawn in the world render. Hover a row to outline the prop on the map.");
    }

    fn map_view(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.map_rect = rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(18));
        let Some(map) = &self.map else {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No map loaded", Default::default(), Color32::GRAY);
            return;
        };

        if self.fit_pending {
            let [x0, y0, x1, y1] = map.bounds;
            let zoom = (rect.width() / (x1 - x0).max(1.0)).min(rect.height() / (y1 - y0).max(1.0)) * 0.95;
            self.view = View { center: [(x0 + x1) / 2.0, (y0 + y1) / 2.0], zoom };
            self.fit_pending = false;
        }

        let visible = &self.visible;
        let dragging_waypoint = self.waypointer.interact(ui, &response, rect, self.view, |p, prefer| {
            locate(&map.data, visible, p, prefer).map(|(plane, _)| plane as u32)
        });

        // Pan with the primary (unless it drags a waypoint) or middle
        // button, zoom around the cursor.
        let primary = response.dragged_by(egui::PointerButton::Primary) && !dragging_waypoint;
        if primary || response.dragged_by(egui::PointerButton::Middle) {
            let d = response.drag_delta();
            self.view.center[0] -= d.x / self.view.zoom;
            self.view.center[1] += d.y / self.view.zoom;
        }
        let hover = response.hover_pos();
        if let Some(pos) = hover {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = (scroll / 300.0).exp() * pinch;
            if factor != 1.0 {
                let before = self.view.to_world(rect, pos);
                self.view.zoom = (self.view.zoom * factor).clamp(1e-4, 50.0);
                let after = self.view.to_world(rect, pos);
                self.view.center[0] += before[0] - after[0];
                self.view.center[1] += before[1] - after[1];
            }
        }

        let view = self.view;
        if self.settings.show_background
            && let Some(background) = &map.background
        {
            background.paint(&painter, rect, view, self.settings.background_opacity);
        }
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            DrawMap {
                generation: map.generation,
                center: view.center,
                scale: [2.0 * view.zoom / rect.width(), 2.0 * view.zoom / rect.height()],
                visible: self.visible.clone(),
                fills: self.settings.show_fills,
                walls: self.settings.show_walls,
                splits: self.settings.show_splits,
            },
        ));

        if self.settings.show_start_points {
            for (i, plane) in map.data.planes.iter().enumerate().filter(|(i, _)| self.visible[*i]) {
                for p in &plane.start_points {
                    let s = view.to_screen(rect, *p);
                    painter.circle(s, 4.0, plane_color(i), Stroke::new(1.5, Color32::WHITE));
                }
            }
        }

        let zones = &self.zones;
        let name = |id: u32| zones.iter().find(|z| z.mapid == Some(id)).and_then(|z| z.name.clone());
        if let Some(a) = &map.annotations {
            annotations::paint(&painter, rect, view, a, &self.settings.layers, &name);
        }
        self.waypointer.paint(&painter, rect, view);
        if let Some(background) = &map.background
            && let Some(pixels) = self.hovered_prop.and_then(|p| background.sprite_pixels(p))
        {
            let outline = screen_rect(rect, view, background.world_rect(pixels)).expand(2.0);
            painter.rect_stroke(outline, 2.0, Stroke::new(2.0, Color32::YELLOW), egui::StrokeKind::Outside);
        }

        let Some(pos) = hover else { return };
        let world = view.to_world(rect, pos);
        let hit = locate(&map.data, &self.visible, world, None);
        let mut text = format!("x {:.1}  y {:.1}", world[0], world[1]);
        if let Some(a) = &map.annotations
            && let Some(what) = annotations::hover(rect, view, a, &self.settings.layers, pos, &name)
        {
            text += &format!("\n{what}");
        }
        if let Some((i, w)) = self.waypointer.hovered() {
            text += &format!("\nwaypoint {i} (plane {})", w.plane);
        }
        if let Some((plane, index)) = hit {
            let t = &map.data.planes[plane].trapezoids[index];
            if self.settings.show_hover_outline {
                let outline: Vec<Pos2> = corners(t).iter().map(|c| view.to_screen(rect, *c)).collect();
                painter.add(egui::Shape::closed_line(outline, Stroke::new(2.0, Color32::WHITE)));
            }
            text += &format!("\nplane {plane}  trapezoid {index}");
            for (side, portal) in [("left", t.portal_left), ("right", t.portal_right)] {
                if portal != NONE_U16 {
                    let p = &map.data.planes[plane].portals[portal as usize];
                    text += &format!("\n{side} portal {portal} -> plane {} (pair {})", p.neighbor_plane, p.pair);
                }
            }
        }
        painter.text(
            rect.left_top() + Vec2::new(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::monospace(13.0),
            Color32::WHITE,
        );
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.waypoint_window = self.waypointer.open;
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_events(&ui.ctx().clone());
        let mapid = self.loaded_mapid();
        self.waypointer.set_map(mapid);
        egui::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if !self.settings.show_maps_panel && ui.button("▶ Maps").on_hover_text("Show the Maps panel").clicked() {
                    self.settings.show_maps_panel = true;
                }
                if self.source.busy.is_some() {
                    ui.spinner();
                }
                ui.label(&self.status);
                if let Some(f) = self.progress {
                    ui.add(egui::ProgressBar::new(f).desired_width(200.0).show_percentage());
                }
                if !self.settings.show_layers_panel {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Layers ◀").on_hover_text("Show the Layers panel").clicked() {
                            self.settings.show_layers_panel = true;
                        }
                    });
                }
            });
        });
        if self.settings.show_maps_panel {
            egui::Panel::left("maps").resizable(true).default_size(320.0).show(ui, |ui| self.maps_panel(ui));
        }
        if self.settings.show_layers_panel {
            egui::Panel::right("layers").resizable(true).default_size(220.0).show(ui, |ui| self.layers_panel(ui));
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.map_view(ui));
        let default_pos = self.map_rect.right_top() + Vec2::new(-340.0, 40.0);
        self.waypointer.window(ui.ctx(), default_pos, self.map.as_ref().map(|m| &m.nav));
    }
}

/// The topmost visible plane containing `p` (trying `prefer` first) and
/// the index of the trapezoid there.
fn locate(data: &PathingData, visible: &[bool], p: [f32; 2], prefer: Option<u32>) -> Option<(usize, usize)> {
    let find = |i: usize| {
        let plane = data.planes.get(i).filter(|_| visible.get(i).copied().unwrap_or(false))?;
        Some((i, plane.trapezoids.iter().position(|t| contains(t, p))?))
    };
    prefer.and_then(|i| find(i as usize)).or_else(|| (0..data.planes.len()).rev().find_map(find))
}

/// `true` if `p` lies inside the trapezoid (edges included).
fn contains(t: &Trapezoid, [x, y]: [f32; 2]) -> bool {
    if y < t.y_bottom || y > t.y_top {
        return false;
    }
    let f = if t.y_top > t.y_bottom { (y - t.y_bottom) / (t.y_top - t.y_bottom) } else { 0.0 };
    let left = t.x_bottom_left + (t.x_top_left - t.x_bottom_left) * f;
    let right = t.x_bottom_right + (t.x_top_right - t.x_bottom_right) * f;
    x >= left && x <= right
}

/// Bounding box of all trapezoids: `[min_x, min_y, max_x, max_y]`.
fn bounds(data: &PathingData) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for t in data.planes.iter().flat_map(|p| &p.trapezoids) {
        for [x, y] in corners(t) {
            if x.is_finite() && y.is_finite() {
                b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
            }
        }
    }
    if b[0] > b[2] { [-1000.0, -1000.0, 1000.0, 1000.0] } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_roundtrip() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
        let view = View { center: [100.0, -50.0], zoom: 0.25 };
        let w = [1234.5, -678.0];
        let back = view.to_world(rect, view.to_screen(rect, w));
        assert!((back[0] - w[0]).abs() < 1e-2 && (back[1] - w[1]).abs() < 1e-2);
        // World y up is screen y down.
        assert!(view.to_screen(rect, [100.0, 0.0]).y < view.to_screen(rect, [100.0, -100.0]).y);
    }

    #[test]
    fn trapezoid_contains() {
        let t = Trapezoid {
            neighbors: [u32::MAX; 4],
            portal_left: NONE_U16,
            portal_right: NONE_U16,
            y_top: 10.0,
            y_bottom: 0.0,
            x_top_left: 2.0,
            x_top_right: 8.0,
            x_bottom_left: 0.0,
            x_bottom_right: 10.0,
        };
        assert!(contains(&t, [5.0, 5.0]));
        assert!(contains(&t, [1.5, 5.0]));
        assert!(!contains(&t, [0.5, 5.0]));
        assert!(!contains(&t, [5.0, 11.0]));
    }
}
