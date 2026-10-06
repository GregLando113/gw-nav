//! The waypointer: a list of waypoints that can run across maps, edited on
//! the map (right-click to add, drag to move, Delete to remove) and in its
//! window (list, copy, paste, clear, and filling in the shortest path
//! between waypoints). The list survives map changes; only the loaded map's
//! waypoints are drawn and edited on the map. Text format:
//! [`gw_nav::waypoint`].

use eframe::egui::{self, Color32, Pos2, Rect, Response, RichText, Stroke, Vec2};
use gw_nav::pathfind::{NavMesh, RouteError};
use gw_nav::waypoint::{self, MapWaypoint, Waypoint};

use crate::app::View;
use crate::clipboard::ClipboardReader;
use crate::render::plane_color;

/// Screen distance (points) within which the pointer is over a waypoint.
const HIT_RADIUS: f32 = 8.0;
const COLOR: Color32 = Color32::from_rgb(255, 140, 40);

pub struct Waypointer {
    pub points: Vec<MapWaypoint>,
    pub open: bool,
    /// The loaded map's id, if known: the map whose waypoints are shown.
    mapid: Option<u32>,
    /// The waypoint being dragged, and its offset from the pointer in world
    /// units (so it doesn't jump to the pointer when grabbed off-centre).
    dragging: Option<(usize, [f32; 2])>,
    /// The waypoint under the pointer on the map.
    hovered: Option<usize>,
    /// The waypoint whose row is under the pointer in the list.
    highlighted: Option<usize>,
    /// Result of the last action, and whether it is an error.
    message: Option<(String, bool)>,
    clipboard: ClipboardReader,
}

impl Waypointer {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            open: true,
            mapid: None,
            dragging: None,
            hovered: None,
            highlighted: None,
            message: None,
            clipboard: ClipboardReader::new(),
        }
    }

    /// Show the waypoints of map `mapid` (none if it is unknown).
    pub fn set_map(&mut self, mapid: Option<u32>) {
        if mapid != self.mapid {
            self.mapid = mapid;
            self.reset_indices();
        }
    }

    /// Whether waypoint `i` is on the loaded map.
    fn on_map(&self, i: usize) -> bool {
        self.points.get(i).is_some_and(|w| Some(w.mapid) == self.mapid)
    }

    /// The waypoint under the pointer on the map.
    pub fn hovered(&self) -> Option<(usize, &Waypoint)> {
        let i = self.dragging.map(|(i, _)| i).or(self.hovered)?;
        Some((i, &self.points.get(i)?.point))
    }

    /// The waypoint on the loaded map nearest to screen position `pos`, if
    /// close enough.
    fn hit(&self, view: View, rect: Rect, pos: Pos2) -> Option<usize> {
        self.points
            .iter()
            .enumerate()
            .filter(|&(i, _)| self.on_map(i))
            .map(|(i, w)| (i, view.to_screen(rect, w.point.pos()).distance(pos)))
            .filter(|(_, d)| *d <= HIT_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// Forget indices into the list, after it changes.
    fn reset_indices(&mut self) {
        self.dragging = None;
        self.hovered = None;
        self.highlighted = None;
    }

    fn remove(&mut self, i: usize) {
        self.points.remove(i);
        self.reset_indices();
    }

    fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }

    /// Map input. `locate` gives the plane at a world point, trying the
    /// given plane first. Returns `true` while a waypoint is being dragged,
    /// when the view must not pan.
    pub fn interact(
        &mut self,
        ui: &egui::Ui,
        response: &Response,
        rect: Rect,
        view: View,
        locate: impl Fn([f32; 2], Option<u32>) -> Option<u32>,
    ) -> bool {
        use egui::PointerButton::Primary;

        self.hovered = response.hover_pos().and_then(|p| self.hit(view, rect, p));

        if response.drag_started_by(Primary) {
            let origin = ui.input(|i| i.pointer.press_origin());
            self.dragging = origin.and_then(|o| {
                let i = self.hit(view, rect, o)?;
                let (at, w) = (view.to_world(rect, o), self.points[i].point);
                Some((i, [w.x - at[0], w.y - at[1]]))
            });
        }
        if let Some((i, offset)) = self.dragging {
            if response.dragged_by(Primary)
                && let Some(p) = response.interact_pointer_pos()
                && let Some(w) = self.points.get_mut(i).map(|w| &mut w.point)
            {
                let at = view.to_world(rect, p);
                (w.x, w.y) = (at[0] + offset[0], at[1] + offset[1]);
                if let Some(plane) = locate(w.pos(), Some(w.plane)) {
                    w.plane = plane;
                }
            } else {
                self.dragging = None;
            }
        }

        if response.secondary_clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            self.add(view.to_world(rect, p), &locate);
        }

        if let Some(i) = self.hovered
            && self.dragging.is_none()
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.input(|input| input.key_pressed(egui::Key::Delete))
        {
            self.remove(i);
        }

        if self.dragging.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if self.hovered.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        self.dragging.is_some()
    }

    /// Add a waypoint at world point `[x, y]` on the loaded map, after its
    /// last waypoint (or at the end of the list if it has none).
    fn add(&mut self, [x, y]: [f32; 2], locate: impl Fn([f32; 2], Option<u32>) -> Option<u32>) {
        let Some(mapid) = self.mapid else {
            return self.set_message("This map's id is unknown: pick it in the Maps list to add waypoints", true);
        };
        let at = self.points.iter().rposition(|w| w.mapid == mapid).map_or(self.points.len(), |i| i + 1);
        let plane = locate([x, y], None);
        self.points.insert(at, MapWaypoint { mapid, point: Waypoint { x, y, plane: plane.unwrap_or(0) } });
        self.reset_indices();
        self.message =
            plane.is_none().then(|| (format!("Waypoint {at} is not on a visible plane; plane 0 assumed"), true));
    }

    /// Draw the loaded map's waypoints and the lines linking consecutive
    /// ones.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect, view: View) {
        let screen = |w: &MapWaypoint| view.to_screen(rect, w.point.pos());
        for run in self.points.chunk_by(|a, b| a.mapid == b.mapid) {
            if run.len() > 1 && Some(run[0].mapid) == self.mapid {
                painter.add(egui::Shape::line(run.iter().map(screen).collect(), Stroke::new(2.0, COLOR)));
            }
        }
        let active = self.hovered().map(|(i, _)| i);
        for (i, w) in self.points.iter().enumerate().filter(|&(i, _)| self.on_map(i)) {
            let s = screen(w);
            let hot = active == Some(i) || self.highlighted == Some(i);
            let (radius, stroke) = if hot { (7.0, Color32::WHITE) } else { (5.0, COLOR) };
            painter.circle(s, radius, plane_color(w.point.plane as usize), Stroke::new(2.0, stroke));
            painter.text(
                s + Vec2::new(8.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                i.to_string(),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
    }

    /// Replace the list with waypoints parsed from pasted text. Waypoints
    /// without a mapid go on the loaded map.
    fn import(&mut self, text: Result<String, String>) {
        match text.and_then(|t| waypoint::parse(&t, self.mapid)) {
            Ok(points) if points.is_empty() => self.set_message("No waypoints found in the clipboard", true),
            Ok(points) => {
                let here = points.iter().filter(|w| Some(w.mapid) == self.mapid).count();
                let text = if here == points.len() {
                    format!("Pasted {} waypoints", points.len())
                } else {
                    format!("Pasted {} waypoints, {here} on this map", points.len())
                };
                self.set_message(text, false);
                self.points = points;
                self.reset_indices();
            }
            Err(e) => self.set_message(format!("Paste failed: {e}"), true),
        }
    }

    /// The legs between consecutive waypoints on the loaded map, by the
    /// index of their first waypoint.
    fn legs(&self) -> Vec<usize> {
        (0..self.points.len().saturating_sub(1)).filter(|&i| self.on_map(i) && self.on_map(i + 1)).collect()
    }

    /// The bends of the shortest path from waypoint `i` to the next one.
    fn route(&self, nav: &NavMesh, i: usize) -> Result<Vec<MapWaypoint>, String> {
        let (from, to) = (self.points[i], self.points[i + 1]);
        match nav.route(from.point, to.point) {
            Ok(route) => Ok(route.points[1..route.points.len() - 1]
                .iter()
                .map(|&point| MapWaypoint { mapid: from.mapid, point })
                .collect()),
            Err(RouteError::StartOffMesh) => Err(format!("waypoint {i} is not on a plane")),
            Err(RouteError::GoalOffMesh) => Err(format!("waypoint {} is not on a plane", i + 1)),
            Err(RouteError::Unreachable) => Err(format!("waypoint {} can't be reached from waypoint {i}", i + 1)),
        }
    }

    /// Insert the shortest path over each of `legs` (ascending, from
    /// [`Self::legs`]). Nothing changes if any of them fails.
    fn insert_paths(&mut self, nav: &NavMesh, legs: &[usize]) {
        let mut routes = Vec::new();
        for &i in legs {
            match self.route(nav, i) {
                Ok(bends) => routes.push((i, bends)),
                Err(e) => return self.set_message(format!("No path: {e}"), true),
            }
        }
        let inserted: usize = routes.iter().map(|(_, bends)| bends.len()).sum();
        for (i, bends) in routes.into_iter().rev() {
            self.points.splice(i + 1..i + 1, bends);
        }
        self.reset_indices();
        let what = match legs {
            [i] => format!("between waypoints {i} and {}", i + inserted + 1),
            _ => format!("over {} legs", legs.len()),
        };
        self.set_message(format!("Inserted {inserted} waypoints {what}"), false);
    }

    /// The waypoint window, and pastes (clipboard reads and Ctrl+V outside
    /// text fields). `default_pos` places the window the first time; `nav`
    /// is the loaded map, for paths.
    pub fn window(&mut self, ctx: &egui::Context, default_pos: Pos2, nav: Option<&NavMesh>) {
        if let Some(text) = self.clipboard.poll() {
            self.import(text);
        }
        if !ctx.egui_wants_keyboard_input() {
            let pasted = ctx.input(|i| {
                i.events.iter().rev().find_map(|e| match e {
                    egui::Event::Paste(text) => Some(text.clone()),
                    _ => None,
                })
            });
            if let Some(text) = pasted {
                self.import(Ok(text));
            }
        }

        let mut open = self.open;
        egui::Window::new("Waypoints")
            .open(&mut open)
            .default_pos(default_pos)
            .default_width(360.0)
            .default_height(360.0)
            .show(ctx, |ui| self.window_contents(ui, nav));
        self.open = open;
    }

    fn window_contents(&mut self, ui: &mut egui::Ui, nav: Option<&NavMesh>) {
        ui.label(
            RichText::new(
                "Right-click the map to add a waypoint, drag one to move it, Delete while hovering removes it. \
                 The list keeps every map's waypoints; the map shows the loaded map's.",
            )
            .small()
            .weak(),
        );
        let empty = self.points.is_empty();
        let legs = self.legs();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!empty, egui::Button::new("Copy"))
                .on_hover_text("Copy the list, with every map's waypoints, to the clipboard")
                .clicked()
            {
                ui.ctx().copy_text(waypoint::format(&self.points));
                self.set_message(format!("Copied {} waypoints", self.points.len()), false);
            }
            if ui
                .button("Paste")
                .on_hover_text("Replace the list with waypoints from the clipboard (or press Ctrl+V over the map)")
                .clicked()
            {
                self.clipboard.request(ui.ctx());
            }
            if ui.add_enabled(!empty, egui::Button::new("Clear")).on_hover_text("Clear every map's waypoints").clicked()
            {
                self.points.clear();
                self.reset_indices();
                self.set_message("Cleared", false);
            }
            if ui
                .add_enabled(nav.is_some() && !legs.is_empty(), egui::Button::new("Path all"))
                .on_hover_text("Insert the shortest path between every pair of consecutive waypoints on this map")
                .clicked()
                && let Some(nav) = nav
            {
                self.insert_paths(nav, &legs);
            }
        });
        if let Some((text, error)) = &self.message {
            let color = if *error { Color32::LIGHT_RED } else { ui.visuals().weak_text_color() };
            ui.colored_label(color, text);
        }
        let here = (0..self.points.len()).filter(|&i| self.on_map(i)).count();
        ui.label(format!(
            "{} waypoints, {here} on this map, length {:.0}",
            self.points.len(),
            waypoint::length(&self.points)
        ));
        ui.separator();

        let active = self.hovered().map(|(i, _)| i);
        let mut highlighted = None;
        let mut remove = None;
        let mut path_from = None;
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("waypoint list").striped(true).num_columns(7).show(ui, |ui| {
                for text in ["#", "map", "x", "y", "plane", "", ""] {
                    ui.strong(text);
                }
                ui.end_row();
                for (i, w) in self.points.iter().enumerate() {
                    let on_map = self.on_map(i);
                    // Other maps' waypoints are dimmed.
                    let cell = |text: RichText| if on_map { text } else { text.weak() };
                    let mut index = cell(RichText::new(i.to_string()));
                    if active == Some(i) {
                        index = index.strong().color(COLOR);
                    }
                    let row = ui
                        .label(index)
                        .union(ui.label(cell(RichText::new(w.mapid.to_string()))))
                        .union(ui.label(cell(RichText::new(format!("{:.2}", w.point.x)).monospace())))
                        .union(ui.label(cell(RichText::new(format!("{:.2}", w.point.y)).monospace())))
                        .union(ui.label(cell(RichText::new(w.point.plane.to_string()))));
                    let path = ui
                        .add_enabled(nav.is_some() && legs.contains(&i), egui::Button::new("->").small())
                        .on_hover_text("Insert the shortest path to the next waypoint");
                    if path.clicked() {
                        path_from = Some(i);
                    }
                    let delete = ui.small_button("🗑").on_hover_text("Delete");
                    if delete.clicked() {
                        remove = Some(i);
                    }
                    if on_map && ui.rect_contains_pointer(row.rect.union(path.rect).union(delete.rect)) {
                        highlighted = Some(i);
                    }
                    ui.end_row();
                }
            });
        });
        self.highlighted = highlighted;
        if let Some(i) = remove {
            self.remove(i);
        }
        if let (Some(i), Some(nav)) = (path_from, nav) {
            self.insert_paths(nav, &[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Event, Key, Modifiers, PointerButton, RawInput, Sense};

    use super::*;

    const VIEW: View = View { center: [0.0, 0.0], zoom: 1.0 };
    /// The loaded map's id.
    const MAP: u32 = 7;

    /// A map view driven by synthetic input. Plane 1 covers x > 0, plane 0
    /// the rest, except that y > 200 is on no plane.
    struct Harness {
        ctx: egui::Context,
        wp: Waypointer,
        rect: Rect,
        time: f64,
        dragging: bool,
    }

    impl Harness {
        fn new() -> Self {
            let mut wp = Waypointer::new();
            wp.open = false;
            wp.set_map(Some(MAP));
            let mut h = Self { ctx: egui::Context::default(), wp, rect: Rect::NOTHING, time: 0.0, dragging: false };
            h.frame(vec![]);
            h
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.time += 1.0 / 60.0;
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let (wp, rect, dragging) = (&mut self.wp, &mut self.rect, &mut self.dragging);
            let mut output = self.ctx.run_ui(input, |ui| {
                let (r, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
                *rect = r;
                *dragging = wp.interact(ui, &response, r, VIEW, |[x, y], _| (y <= 200.0).then_some(u32::from(x > 0.0)));
                wp.paint(ui.painter(), r, VIEW);
                wp.window(ui.ctx(), Pos2::new(600.0, 10.0), None);
            });
            // There is no renderer to upload textures to.
            output.textures_delta.clear();
        }

        fn at(&self, world: [f32; 2]) -> Pos2 {
            VIEW.to_screen(self.rect, world)
        }

        fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
            Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE }
        }

        fn right_click(&mut self, world: [f32; 2]) {
            let pos = self.at(world);
            self.frame(vec![Event::PointerMoved(pos)]);
            self.frame(vec![Self::button(pos, PointerButton::Secondary, true)]);
            self.frame(vec![Self::button(pos, PointerButton::Secondary, false)]);
            self.frame(vec![]);
        }

        /// Drag with the primary button; returns whether a waypoint drag was
        /// reported while moving.
        fn drag(&mut self, from: Pos2, to: Pos2) -> bool {
            self.frame(vec![Event::PointerMoved(from)]);
            self.frame(vec![Self::button(from, PointerButton::Primary, true)]);
            let mut dragging = false;
            for k in 1..=10 {
                self.frame(vec![Event::PointerMoved(from + (to - from) * (k as f32 / 10.0))]);
                dragging |= self.dragging;
            }
            self.frame(vec![Self::button(to, PointerButton::Primary, false)]);
            self.frame(vec![]);
            dragging
        }

        fn key(&mut self, key: Key) {
            let event =
                |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
            self.frame(vec![event(true)]);
            self.frame(vec![event(false)]);
        }
    }

    /// A waypoint on the loaded map.
    fn wp(x: f32, y: f32, plane: u32) -> MapWaypoint {
        on(MAP, x, y, plane)
    }

    fn on(mapid: u32, x: f32, y: f32, plane: u32) -> MapWaypoint {
        MapWaypoint { mapid, point: Waypoint { x, y, plane } }
    }

    #[test]
    fn right_click_adds_on_the_plane_under_it() {
        let mut h = Harness::new();
        h.right_click([-100.0, 50.0]);
        h.right_click([120.0, -30.0]);
        assert_eq!(h.wp.points, vec![wp(-100.0, 50.0, 0), wp(120.0, -30.0, 1)]);
        assert!(h.wp.message.is_none());

        h.right_click([0.0, 250.0]);
        assert_eq!(h.wp.points[2], wp(0.0, 250.0, 0));
        assert!(h.wp.message.as_ref().is_some_and(|(m, error)| *error && m.contains("not on a visible plane")));
    }

    #[test]
    fn drag_moves_a_waypoint_instead_of_panning() {
        let mut h = Harness::new();
        h.right_click([-100.0, 0.0]);
        h.right_click([-50.0, -50.0]);
        // Grab 3 points off-centre; the waypoint keeps that offset.
        let grab = h.at([-97.0, 0.0]);
        assert!(h.drag(grab, grab + Vec2::new(200.0, 40.0)));
        assert_eq!(h.wp.points, vec![wp(100.0, -40.0, 1), wp(-50.0, -50.0, 0)]);
        assert!(h.wp.dragging.is_none());

        // Away from waypoints a drag is left to the view.
        let empty = h.at([-300.0, 100.0]);
        assert!(!h.drag(empty, empty + Vec2::new(50.0, 0.0)));
        assert_eq!(h.wp.points.len(), 2);
    }

    #[test]
    fn delete_removes_the_hovered_waypoint() {
        let mut h = Harness::new();
        for x in [-100.0, 0.0, 100.0] {
            h.right_click([x, 0.0]);
        }
        h.frame(vec![Event::PointerMoved(h.at([-150.0, 100.0]))]);
        h.key(Key::Delete);
        assert_eq!(h.wp.points.len(), 3, "nothing hovered");
        h.frame(vec![Event::PointerMoved(h.at([4.0, -3.0]))]);
        assert_eq!(h.wp.hovered().map(|(i, _)| i), Some(1));
        h.key(Key::Delete);
        assert_eq!(h.wp.points, vec![wp(-100.0, 0.0, 0), wp(100.0, 0.0, 1)]);
    }

    /// A 10×10 room with a wall from the bottom up to y = 8 at x 4..6, and
    /// a separate 1×1 room at x 20.
    fn room() -> NavMesh {
        use gw_nav::mapfile::navmesh::{NONE_U16, NONE_U32, NodeRef, PathPlane, Trapezoid};
        let trap = |y: [f32; 2], x: [f32; 2], n: [u32; 4]| Trapezoid {
            neighbors: n,
            portal_left: NONE_U16,
            portal_right: NONE_U16,
            y_top: y[1],
            y_bottom: y[0],
            x_top_left: x[0],
            x_top_right: x[1],
            x_bottom_left: x[0],
            x_bottom_right: x[1],
        };
        let none = NONE_U32;
        NavMesh::new(&[PathPlane {
            start_points: vec![],
            vectors: vec![],
            trapezoids: vec![
                trap([0.0, 8.0], [0.0, 4.0], [2, none, none, none]),
                trap([0.0, 8.0], [6.0, 10.0], [2, none, none, none]),
                trap([8.0, 10.0], [0.0, 10.0], [none, none, 0, 1]),
                trap([0.0, 1.0], [20.0, 21.0], [none; 4]),
            ],
            root: NodeRef::None,
            x_nodes: vec![],
            y_nodes: vec![],
            sinks: vec![],
            portal_trapezoids: vec![],
            portals: vec![],
        }])
    }

    #[test]
    fn inserts_paths_between_waypoints() {
        let nav = room();
        let mut w = Waypointer::new();
        w.set_map(Some(MAP));
        w.points = vec![wp(1.0, 1.0, 0), wp(9.0, 1.0, 0), wp(9.0, 9.0, 0)];
        w.insert_paths(&nav, &[0]);
        assert_eq!(w.points, vec![wp(1.0, 1.0, 0), wp(4.0, 8.0, 0), wp(6.0, 8.0, 0), wp(9.0, 1.0, 0), wp(9.0, 9.0, 0)]);
        assert_eq!(w.message.as_ref().unwrap().0, "Inserted 2 waypoints between waypoints 0 and 3");

        // Already straight legs gain nothing.
        w.insert_paths(&nav, &w.legs());
        assert_eq!(w.points.len(), 5);

        // One failing leg leaves the list alone.
        w.points.push(wp(20.5, 0.5, 0));
        w.points.insert(1, wp(9.0, 1.0, 0));
        let before = w.points.clone();
        w.insert_paths(&nav, &w.legs());
        assert_eq!(w.points, before);
        assert!(w.message.as_ref().is_some_and(|(m, error)| *error && m.contains("waypoint 6 can't be reached")));

        // Legs into or out of another map's waypoints are not pathed.
        w.points = vec![wp(1.0, 1.0, 0), on(8, 9.0, 1.0, 0), wp(9.0, 1.0, 0), wp(9.0, 9.0, 0)];
        assert_eq!(w.legs(), vec![2]);
    }

    #[test]
    fn paste_replaces_the_list() {
        let mut h = Harness::new();
        h.wp.open = true;
        h.right_click([-100.0, 0.0]);
        let text = waypoint::format(&[wp(1.5, 2.0, 3), on(8, -4.0, 5.25, 0)]);
        h.frame(vec![Event::Paste(text)]);
        assert_eq!(h.wp.points, vec![wp(1.5, 2.0, 3), on(8, -4.0, 5.25, 0)]);
        assert!(h.wp.message.as_ref().is_some_and(|(m, error)| !*error && m == "Pasted 2 waypoints, 1 on this map"));
        // Waypoints without a mapid go on the loaded map.
        h.frame(vec![Event::Paste("{ x = 1, y = 1 }".into())]);
        assert_eq!(h.wp.points, vec![wp(1.0, 1.0, 0)]);
        // Text without waypoints leaves the list alone.
        h.frame(vec![Event::Paste("hello".into())]);
        assert_eq!(h.wp.points.len(), 1);
        assert!(h.wp.message.as_ref().is_some_and(|(_, error)| *error));
        h.frame(vec![]);
    }

    #[test]
    fn keeps_other_maps_waypoints_but_only_edits_the_loaded_maps() {
        let mut h = Harness::new();
        h.right_click([-100.0, 0.0]);
        h.right_click([100.0, 0.0]);

        // On map 8, map 7's waypoints can't be hovered, dragged or deleted.
        h.wp.set_map(Some(8));
        h.frame(vec![Event::PointerMoved(h.at([-100.0, 0.0]))]);
        assert!(h.wp.hovered().is_none());
        h.key(Key::Delete);
        let grab = h.at([100.0, 0.0]);
        assert!(!h.drag(grab, grab + Vec2::new(0.0, 50.0)));
        h.right_click([0.0, 50.0]);
        assert_eq!(h.wp.points, vec![wp(-100.0, 0.0, 0), wp(100.0, 0.0, 1), on(8, 0.0, 50.0, 0)]);

        // Back on map 7, a new waypoint follows map 7's last one.
        h.wp.set_map(Some(MAP));
        h.right_click([-50.0, -50.0]);
        assert_eq!(h.wp.points[2], wp(-50.0, -50.0, 0));
        assert_eq!(h.wp.points[3], on(8, 0.0, 50.0, 0));
        h.frame(vec![Event::PointerMoved(h.at([100.0, 0.0]))]);
        assert_eq!(h.wp.hovered().map(|(i, _)| i), Some(1));

        // Without a map id nothing can be added.
        h.wp.set_map(None);
        h.right_click([0.0, 0.0]);
        assert_eq!(h.wp.points.len(), 4);
        assert!(h.wp.message.as_ref().is_some_and(|(m, error)| *error && m.contains("map's id is unknown")));
    }
}
