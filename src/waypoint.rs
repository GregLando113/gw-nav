//! Waypoint lists, which can run across maps, and their text format, one
//! waypoint per line:
//!
//! ```text
//! { x = -1234.50000, y = 678.25000, plane = 0, mapid = 7 },
//! ```
//!
//! (C format `{ x = %5.5f, y = %5.5f, plane = %d, mapid = %d },\n`), which
//! pastes directly into a Lua table.

use std::fmt::Write as _;

/// A point on a pathing plane, in world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Waypoint {
    pub x: f32,
    pub y: f32,
    /// Index of the plane in the map's plane list (0 is the ground).
    pub plane: u32,
}

impl Waypoint {
    pub fn pos(&self) -> [f32; 2] {
        [self.x, self.y]
    }
}

/// A waypoint on a map: the map's id (`mapid` in MapDb) and the point on
/// its pathing planes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapWaypoint {
    pub mapid: u32,
    pub point: Waypoint,
}

/// Format waypoints, one line each.
pub fn format(waypoints: &[MapWaypoint]) -> String {
    let mut out = String::new();
    for MapWaypoint { mapid, point: w } in waypoints {
        let _ = writeln!(out, "{{ x = {:.5}, y = {:.5}, plane = {}, mapid = {mapid} }},", w.x, w.y, w.plane);
    }
    out
}

/// Parse waypoints from text in the [`format`] format.
///
/// Lenient about layout: every innermost `{ ... }` group is one waypoint
/// (so a list wrapped in an outer Lua table works too), with `key = value`
/// fields separated by commas in any order. `x` and `y` are required;
/// `plane` defaults to 0, `mapid` to `default_mapid` (an error if that is
/// `None`), and other fields are ignored.
pub fn parse(text: &str, default_mapid: Option<u32>) -> Result<Vec<MapWaypoint>, String> {
    let mut waypoints = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match c {
            '{' => start = Some(i + 1),
            '}' => {
                if let Some(s) = start.take() {
                    let body = &text[s..i];
                    if !body.trim().is_empty() {
                        let n = waypoints.len() + 1;
                        let w = parse_one(body, default_mapid).map_err(|e| format!("waypoint {n} `{{{body}}}`: {e}"))?;
                        waypoints.push(w);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(waypoints)
}

fn parse_one(body: &str, default_mapid: Option<u32>) -> Result<MapWaypoint, String> {
    let (mut x, mut y, mut plane, mut mapid) = (None, None, None, None);
    for field in body.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        let (key, value) = field.split_once('=').ok_or_else(|| format!("expected `key = value`, got `{field}`"))?;
        let (key, value) = (key.trim(), value.trim());
        let number = |v: &str| v.parse::<f32>().ok().filter(|v| v.is_finite());
        match key {
            "x" => x = Some(number(value).ok_or_else(|| format!("bad x `{value}`"))?),
            "y" => y = Some(number(value).ok_or_else(|| format!("bad y `{value}`"))?),
            "plane" => plane = Some(value.parse::<u32>().map_err(|_| format!("bad plane `{value}`"))?),
            "mapid" => mapid = Some(value.parse::<u32>().map_err(|_| format!("bad mapid `{value}`"))?),
            _ => {}
        }
    }
    let point = Waypoint { x: x.ok_or("missing x")?, y: y.ok_or("missing y")?, plane: plane.unwrap_or(0) };
    Ok(MapWaypoint { mapid: mapid.or(default_mapid).ok_or("missing mapid")?, point })
}

/// Total length of the path through the waypoints, ignoring planes and
/// the moves between maps.
pub fn length(waypoints: &[MapWaypoint]) -> f32 {
    // A fold from +0.0: an empty `sum` of floats is -0.0.
    waypoints
        .windows(2)
        .filter(|w| w[0].mapid == w[1].mapid)
        .map(|w| (w[1].point.x - w[0].point.x).hypot(w[1].point.y - w[0].point.y))
        .fold(0.0, |a, d| a + d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mw(x: f32, y: f32, plane: u32, mapid: u32) -> MapWaypoint {
        MapWaypoint { mapid, point: Waypoint { x, y, plane } }
    }

    #[test]
    fn formats_like_printf() {
        let w = [mw(-1234.5, 678.25, 0, 7), mw(0.0, 1.0, 12, 248)];
        assert_eq!(
            format(&w),
            "{ x = -1234.50000, y = 678.25000, plane = 0, mapid = 7 },\n\
             { x = 0.00000, y = 1.00000, plane = 12, mapid = 248 },\n"
        );
    }

    #[test]
    fn roundtrip() {
        let w = vec![mw(-16164.125, 20552.5, 3, 248), mw(1.5, -2.0, 0, 7)];
        assert_eq!(parse(&format(&w), None).unwrap(), w);
    }

    #[test]
    fn lenient_layout() {
        let text = "local path = {\n  {x=1, y = 2},{ plane = 4, y=-3.5e2, x = 5.0 , extra = true, mapid = 9 }\n}\n";
        assert_eq!(parse(text, Some(3)).unwrap(), vec![mw(1.0, 2.0, 0, 3), mw(5.0, -350.0, 4, 9)]);
        assert_eq!(parse("", None).unwrap(), vec![]);
        assert_eq!(parse("{}", None).unwrap(), vec![]);
    }

    #[test]
    fn rejects_bad_entries() {
        assert!(parse("{ x = 1 }", Some(1)).unwrap_err().contains("missing y"));
        assert!(parse("{ x = 1, y = 2 }, { x = a, y = 2 }", Some(1)).unwrap_err().starts_with("waypoint 2"));
        assert!(parse("{ x = 1, y = 2, plane = -1 }", Some(1)).is_err());
        assert!(parse("{ x = 1 y = 2 }", Some(1)).is_err());
        assert!(parse("{ x = 1, y = 2, mapid = x }", Some(1)).is_err());
        assert!(parse("{ x = 1, y = 2, mapid = 3 }, { x = 1, y = 2 }", None).unwrap_err().contains("missing mapid"));
    }

    #[test]
    fn path_length() {
        let w = [mw(0.0, 0.0, 0, 1), mw(3.0, 4.0, 1, 1), mw(100.0, 100.0, 0, 2), mw(100.0, 101.0, 0, 2)];
        assert_eq!(length(&w), 6.0);
        assert!(length(&w[..1]).is_sign_positive() && length(&[]) == 0.0);
    }
}
