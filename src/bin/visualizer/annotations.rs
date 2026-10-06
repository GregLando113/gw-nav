//! The mission-point, portal-prop and zone-exit layers
//! ([`MapAnnotations`]). The Zones chunk is drawn by `zone_chunk`.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};
use gw_nav::api::MapAnnotations;
use gw_nav::mapfile::mission::MissionPoint;

use crate::app::View;

/// Spawns land this far (world units) from their mission point.
const SPAWN_RADIUS: f32 = 75.0;
/// Screen distance (points) within which the pointer is over a point.
const HIT_RADIUS: f32 = 9.0;

const OWN_SPAWN: Color32 = Color32::from_rgb(90, 220, 120);
const MAP_SPAWN: Color32 = Color32::from_rgb(80, 200, 255);
const NAMED: Color32 = Color32::from_rgb(255, 190, 60);
const UNNAMED: Color32 = Color32::from_gray(200);
const EXIT: Color32 = Color32::from_rgb(255, 80, 80);
const PORTAL: Color32 = Color32::from_rgb(200, 120, 255);

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Layers {
    pub points: bool,
    pub portals: bool,
    pub exits: bool,
    /// Missing from settings saved before it existed.
    #[serde(default = "on")]
    pub zones: bool,
}

fn on() -> bool {
    true
}

fn color(a: &MapAnnotations, p: &MissionPoint) -> Color32 {
    match p.map_id() {
        Some(id) if a.map_ids.contains(&id) => OWN_SPAWN,
        Some(_) => MAP_SPAWN,
        None if p.tag.is_empty() => UNNAMED,
        None => NAMED,
    }
}

/// Screen direction of a world-space angle (world y is up).
fn screen_dir(angle: f32) -> Vec2 {
    Vec2::new(angle.cos(), -angle.sin())
}

fn map_label(id: u32, name: &impl Fn(u32) -> Option<String>) -> String {
    match name(id) {
        Some(n) => format!("{id} {n}"),
        None => id.to_string(),
    }
}

/// The layer checkboxes, with counts and any notes.
pub fn panel(ui: &mut egui::Ui, a: Option<&MapAnnotations>, layers: &mut Layers) {
    let (points, portals, exits) =
        a.map_or((0, 0, 0), |a| (a.mission_points.len(), a.portal_props.len(), a.zone_exits.len()));
    ui.checkbox(&mut layers.points, format!("Mission points ({points})")).on_hover_text(
        "Named points from the map file's mission chunk. Green: spawns of this map (map travel lands 75 units \
         from one), blue: tagged with another map id, orange: other names, grey: unnamed. The tick is the \
         spawn facing.",
    );
    ui.checkbox(&mut layers.portals, format!("Portal props ({portals})")).on_hover_text(
        "Props drawn with a zone portal model (43045 in Prophecies and Factions maps, 247212 in Nightfall and \
         Eye of the North maps). A few copies are decoration. The bar shows the prop's yaw.",
    );
    ui.checkbox(&mut layers.exits, format!("Zone exits ({exits})"))
        .on_hover_text("Zone transitions recorded in game (gwbs zones.db), with the direction of travel");
    let zones = a.and_then(|a| a.zone_chunk.as_ref()).map_or(0, |c| c.zones.len());
    ui.checkbox(&mut layers.zones, format!("Zones chunk ({zones})")).on_hover_text(
        "The map file's Zones chunk: areas the client fills with grass, trees and rocks, outlined in the colour \
         of their def. Not zone (map) transitions. Defs can be hidden in the Zones list.",
    );
    for note in a.map_or(&[][..], |a| &a.notes[..]) {
        ui.colored_label(ui.visuals().weak_text_color(), egui::RichText::new(note).small());
    }
}

pub fn paint(
    painter: &egui::Painter,
    rect: Rect,
    view: View,
    a: &MapAnnotations,
    layers: &Layers,
    name: &impl Fn(u32) -> Option<String>,
) {
    let font = egui::FontId::proportional(11.0);
    if layers.points {
        for p in &a.mission_points {
            let s = view.to_screen(rect, [p.x, p.y]);
            let c = color(a, p);
            let ring = SPAWN_RADIUS * view.zoom;
            if ring >= 4.0 {
                painter.circle_stroke(s, ring, Stroke::new(1.0, c.gamma_multiply(0.5)));
            }
            painter.line_segment([s, s + screen_dir(p.facing_radians()) * 14.0], Stroke::new(2.0, c));
            let d = 5.0;
            let diamond = vec![s + Vec2::new(0.0, -d), s + Vec2::new(d, 0.0), s + Vec2::new(0.0, d), s + Vec2::new(-d, 0.0)];
            painter.add(egui::Shape::convex_polygon(diamond, c, Stroke::new(1.0, Color32::BLACK)));
            if !p.tag.is_empty() {
                painter.text(s + Vec2::new(7.0, 7.0), egui::Align2::LEFT_TOP, &p.tag, font.clone(), c);
            }
        }
    }
    if layers.portals {
        for p in &a.portal_props {
            let s = view.to_screen(rect, [p.x, p.y]);
            let yaw = p.yaw as f32 * std::f32::consts::TAU / 256.0;
            let bar = screen_dir(yaw) * 11.0;
            painter.line_segment([s - bar, s + bar], Stroke::new(3.0, PORTAL));
            painter.circle(s, 8.0, PORTAL.gamma_multiply(0.25), Stroke::new(2.0, PORTAL));
            painter.text(s + Vec2::new(10.0, 4.0), egui::Align2::LEFT_TOP, "portal", font.clone(), PORTAL);
        }
    }
    if layers.exits {
        for e in &a.zone_exits {
            let s = view.to_screen(rect, [e.x, e.y]);
            if let Some([dx, dy]) = e.dir {
                let tip = s + Vec2::new(dx, -dy).normalized() * 18.0;
                painter.arrow(s, tip - s, Stroke::new(2.0, EXIT));
            }
            painter.circle(s, 6.0, EXIT.gamma_multiply(0.35), Stroke::new(2.0, EXIT));
            let to = e.to_map.map_or("?".to_owned(), |id| map_label(id, name));
            painter.text(s + Vec2::new(8.0, -8.0), egui::Align2::LEFT_BOTTOM, format!("-> {to}"), font.clone(), EXIT);
        }
    }
}

/// A description of the point or exit under `pos`, if any.
pub fn hover(
    rect: Rect,
    view: View,
    a: &MapAnnotations,
    layers: &Layers,
    pos: Pos2,
    name: &impl Fn(u32) -> Option<String>,
) -> Option<String> {
    let near = |x: f32, y: f32| {
        let d = view.to_screen(rect, [x, y]).distance(pos);
        (d <= HIT_RADIUS).then_some(d)
    };
    let points = a.mission_points.iter().filter(|_| layers.points).filter_map(|p| {
        let what = match (p.tag.as_str(), p.map_id()) {
            ("", _) => "unnamed".to_owned(),
            (_, Some(id)) => format!("'{}' (map {})", p.tag, map_label(id, name)),
            (tag, None) => format!("'{tag}'"),
        };
        let facing = p.facing as f32 * 360.0 / 256.0;
        Some((near(p.x, p.y)?, format!("mission point {what}, list {}, facing {facing:.0}°", p.list)))
    });
    let exits = a.zone_exits.iter().filter(|_| layers.exits).filter_map(|e| {
        let to = e.to_map.map_or("?".to_owned(), |id| map_label(id, name));
        let from = map_label(e.from_map, name);
        Some((near(e.x, e.y)?, format!("zone exit {from} -> {to} (plane {}, seen {} times)", e.plane, e.hits)))
    });
    let portals = a.portal_props.iter().filter(|_| layers.portals).filter_map(|p| {
        let yaw = p.yaw as f32 * 360.0 / 256.0;
        Some((near(p.x, p.y)?, format!("portal prop #{} (model {}), yaw {yaw:.0}°", p.prop, p.model)))
    });
    points.chain(portals).chain(exits).min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, text)| text)
}
