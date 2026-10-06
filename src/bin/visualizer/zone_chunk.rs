//! The Zones chunk layer: the map file's procedurally populated areas
//! (grass, trees, rocks) outlined in their def's colour, and the Zones list
//! that groups the defs by the folder of their `.ini` path.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use gw_nav::api::ZoneChunk;
use gw_nav::mapfile::zones::{Zone, ini_folder, ini_stem};

use crate::app::View;

/// Outlines of zones whose def isn't in the chunk.
const NO_DEF: Color32 = Color32::from_gray(160);
/// Zones narrower than this on screen (points) get no label.
const LABEL_WIDTH: f32 = 60.0;

/// The Zones chunk of the loaded map, with which defs are shown.
pub struct ZoneView {
    pub chunk: ZoneChunk,
    /// Whether each def's zones are drawn, by index in `chunk.defs`.
    pub def_visible: Vec<bool>,
    /// The first model of each def in `chunk.model_files`.
    model_starts: Vec<usize>,
}

/// What the Zones list's pointer is over, highlighted on the map.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ListHover {
    pub zone: Option<usize>,
    pub def: Option<usize>,
}

/// The colour of def index `def`.
pub fn def_color(def: usize) -> Color32 {
    let hue = (0.13 + def as f32 * 0.618_034).fract();
    egui::ecolor::Hsva::new(hue, 0.75, 1.0, 1.0).into()
}

impl ZoneView {
    pub fn new(chunk: ZoneChunk) -> Self {
        Self { def_visible: vec![true; chunk.defs.len()], model_starts: chunk.model_starts(), chunk }
    }

    /// The index of a zone's def in `chunk.defs`.
    fn def_of(&self, zone: &Zone) -> Option<usize> {
        self.chunk.defs.iter().position(|d| d.id == zone.def_id)
    }

    fn is_visible(&self, zone: &Zone) -> bool {
        self.def_of(zone).is_none_or(|d| self.def_visible[d])
    }

    fn color(&self, zone: &Zone) -> Color32 {
        self.def_of(zone).map_or(NO_DEF, def_color)
    }

    fn name(&self, zone: &Zone) -> String {
        match self.def_of(zone) {
            Some(d) => ini_stem(&self.chunk.defs[d].ini_path).to_owned(),
            None => format!("def {}", zone.def_id),
        }
    }

    /// The smallest shown zone containing world point `p`.
    pub fn hit(&self, p: [f32; 2]) -> Option<usize> {
        self.chunk
            .zones
            .iter()
            .enumerate()
            .filter(|(_, z)| self.is_visible(z) && z.contains(p))
            .min_by(|a, b| a.1.signed_area().abs().total_cmp(&b.1.signed_area().abs()))
            .map(|(i, _)| i)
    }

    /// The hover text of zone `index`.
    pub fn describe(&self, index: usize) -> String {
        let zone = &self.chunk.zones[index];
        let mut text = format!(
            "zone #{index} · {} (def {}) · flags {:#04x} · height {}",
            self.name(zone),
            zone.def_id,
            zone.flags,
            zone.height()
        );
        if let Some(d) = self.def_of(zone) {
            text += &format!("\n{}", self.chunk.defs[d].ini_path);
        }
        text
    }

    /// Outline the shown zones, fill `highlight`, and draw the zones of
    /// `def_highlight` heavier.
    pub fn paint(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        view: View,
        highlight: Option<usize>,
        def_highlight: Option<usize>,
    ) {
        let font = egui::FontId::proportional(11.0);
        for (i, zone) in self.chunk.zones.iter().enumerate() {
            if !self.is_visible(zone) || zone.vertices.len() < 2 {
                continue;
            }
            let color = self.color(zone);
            let points: Vec<Pos2> = zone.vertices.iter().map(|&v| view.to_screen(rect, v)).collect();
            let screen = Rect::from_points(&points);
            if !screen.intersects(rect) {
                continue;
            }
            let heavy = highlight == Some(i) || (def_highlight.is_some() && self.def_of(zone) == def_highlight);
            if highlight == Some(i) {
                painter.add(fill(&points, color.gamma_multiply(0.35)));
            }
            // Dashed, so they don't read as the annotation markers, which
            // use most hues already; solid when highlighted.
            if heavy {
                painter.add(egui::Shape::closed_line(points, Stroke::new(2.5, color)));
            } else {
                let mut closed = points;
                closed.push(closed[0]);
                painter.extend(egui::Shape::dashed_line(&closed, Stroke::new(1.5, color), 8.0, 4.0));
            }
            if screen.width() > LABEL_WIDTH {
                let at = view.to_screen(rect, zone.centroid());
                painter.text(at, egui::Align2::CENTER_CENTER, self.name(zone), font.clone(), color);
            }
        }
    }

    /// The Zones list: defs grouped by folder, each with its layers, models
    /// and zones. Sets `hover` to the row under the pointer; returns the
    /// centroid of a zone row that was clicked.
    pub fn panel(&mut self, ui: &mut egui::Ui, hover: &mut ListHover) -> Option<[f32; 2]> {
        // Folders in order of first use.
        let mut folders: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, def) in self.chunk.defs.iter().enumerate() {
            let folder = ini_folder(&def.ini_path);
            match folders.iter_mut().find(|(f, _)| *f == folder) {
                Some((_, defs)) => defs.push(i),
                None => folders.push((folder, vec![i])),
            }
        }
        let folders: Vec<(String, Vec<usize>)> = folders.into_iter().map(|(f, d)| (f.to_owned(), d)).collect();
        let mut center = None;
        for (folder, defs) in &folders {
            let title = if folder.is_empty() { "(no folder)" } else { folder.as_str() };
            egui::CollapsingHeader::new(title).id_salt(("zone folder", folder)).default_open(true).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (label, on) in [("all", true), ("none", false)] {
                        if ui.small_button(label).clicked() {
                            defs.iter().for_each(|&d| self.def_visible[d] = on);
                        }
                    }
                });
                for &d in defs {
                    if let Some(c) = self.def_rows(ui, d, hover) {
                        center = Some(c);
                    }
                }
            });
        }
        let orphans: Vec<usize> = (0..self.chunk.zones.len()).filter(|&i| self.def_of(&self.chunk.zones[i]).is_none()).collect();
        for i in orphans {
            if let Some(c) = self.zone_row(ui, i, hover) {
                center = Some(c);
            }
        }
        center
    }

    fn def_rows(&mut self, ui: &mut egui::Ui, d: usize, hover: &mut ListHover) -> Option<[f32; 2]> {
        let def = &self.chunk.defs[d];
        let zones: Vec<usize> = (0..self.chunk.zones.len()).filter(|&i| self.chunk.zones[i].def_id == def.id).collect();
        let (stem, path, id) = (ini_stem(&def.ini_path).to_owned(), def.ini_path.clone(), def.id);
        let state_id = ui.make_persistent_id(("zone def", d, id));
        let mut center = None;
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), state_id, false)
            .show_header(ui, |ui| {
                let (swatch, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                ui.painter().rect_filled(swatch, 2.0, def_color(d));
                let check = ui.checkbox(&mut self.def_visible[d], stem).on_hover_text(&path);
                let detail = ui.weak(format!("def {id} · {} zones · {} layers", zones.len(), def.layers.len()));
                if check.hovered() || detail.hovered() {
                    hover.def = Some(d);
                }
            })
            .body(|ui| {
                let def = &self.chunk.defs[d];
                let mut model = self.model_starts.get(d).copied().unwrap_or(0);
                for (l, (layer, models)) in def.layer_models().enumerate() {
                    ui.label(
                        egui::RichText::new(format!(
                            "layer {l} · level {} · spacing {} · collision {} · density {} · variance {} · pattern {} · kind {}",
                            layer.level(),
                            layer.spacing,
                            layer.collision_radius,
                            layer.density,
                            layer.scale_variance,
                            layer.pattern,
                            layer.kind,
                        ))
                        .small(),
                    );
                    let mut previous = 0.0;
                    for m in models {
                        let file = self.chunk.model_files.get(model).map_or("?".into(), |f| f.to_string());
                        ui.label(
                            egui::RichText::new(format!(
                                "    model {file:>7}  p {:.2}  flags {:#06x}",
                                m.cumulative_probability - previous,
                                m.flags
                            ))
                            .small()
                            .monospace(),
                        );
                        previous = m.cumulative_probability;
                        model += 1;
                    }
                }
                for &i in &zones {
                    if let Some(c) = self.zone_row(ui, i, hover) {
                        center = Some(c);
                    }
                }
            });
        center
    }

    fn zone_row(&self, ui: &mut egui::Ui, i: usize, hover: &mut ListHover) -> Option<[f32; 2]> {
        let zone = &self.chunk.zones[i];
        let area = zone.signed_area().abs();
        let label = format!(
            "zone #{i} · {} vertices · {:.1}M u² · flags {:#04x} · height {}",
            zone.vertices.len(),
            area / 1e6,
            zone.flags,
            zone.height()
        );
        let response = ui.selectable_label(hover.zone == Some(i), label).on_hover_text("Click to centre the map on it");
        if response.hovered() {
            hover.zone = Some(i);
        }
        response.clicked().then(|| zone.centroid())
    }
}

/// A filled polygon (concave ones too).
fn fill(points: &[Pos2], color: Color32) -> egui::Shape {
    let mut mesh = egui::Mesh::default();
    for &p in points {
        mesh.colored_vertex(p, color);
    }
    let coords: Vec<[f32; 2]> = points.iter().map(|p| [p.x, p.y]).collect();
    for [a, b, c] in triangulate(&coords) {
        mesh.add_triangle(a as u32, b as u32, c as u32);
    }
    egui::Shape::mesh(mesh)
}

/// Ear-clipping triangulation of a simple polygon in either winding.
/// Falls back to a fan if no ear is found (a self-intersecting polygon).
pub fn triangulate(points: &[[f32; 2]]) -> Vec<[usize; 3]> {
    let cross = |o: [f32; 2], a: [f32; 2], b: [f32; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let area: f32 = (0..points.len())
        .map(|i| {
            let (a, b) = (points[i], points[(i + 1) % points.len()]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    // Work counter-clockwise.
    let mut left: Vec<usize> = (0..points.len()).collect();
    if area < 0.0 {
        left.reverse();
    }
    let mut out = Vec::with_capacity(points.len().saturating_sub(2));
    while left.len() > 3 {
        let n = left.len();
        let ear = (0..n).find(|&i| {
            let (a, b, c) = (left[(i + n - 1) % n], left[i], left[(i + 1) % n]);
            let (pa, pb, pc) = (points[a], points[b], points[c]);
            cross(pa, pb, pc) > 0.0
                && !left.iter().any(|&j| {
                    j != a
                        && j != b
                        && j != c
                        && cross(pa, pb, points[j]) >= 0.0
                        && cross(pb, pc, points[j]) >= 0.0
                        && cross(pc, pa, points[j]) >= 0.0
                })
        });
        let Some(i) = ear else {
            out.extend((1..n - 1).map(|k| [left[0], left[k], left[k + 1]]));
            return out;
        };
        out.push([left[(i + n - 1) % n], left[i], left[(i + 1) % n]]);
        left.remove(i);
    }
    if left.len() == 3 {
        out.push([left[0], left[1], left[2]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(points: &[[f32; 2]], tris: &[[usize; 3]]) -> f32 {
        tris.iter()
            .map(|&[a, b, c]| {
                let (pa, pb, pc) = (points[a], points[b], points[c]);
                ((pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0])).abs() / 2.0
            })
            .sum()
    }

    #[test]
    fn triangulates_concave_polygons() {
        // A U shape (area 7), clockwise like the zones in map files.
        let u = [[0.0, 0.0], [0.0, 3.0], [1.0, 3.0], [1.0, 1.0], [2.0, 1.0], [2.0, 3.0], [3.0, 3.0], [3.0, 0.0]];
        let tris = triangulate(&u);
        assert_eq!(tris.len(), 6);
        assert_eq!(area(&u, &tris), 7.0);
        let mut ccw = u;
        ccw.reverse();
        assert_eq!(area(&ccw, &triangulate(&ccw)), 7.0);
        assert_eq!(triangulate(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]), [[0, 1, 2]]);
        assert!(triangulate(&[[0.0, 0.0], [1.0, 0.0]]).is_empty());
    }
}
