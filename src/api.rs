//! The relay server's HTTP API, shared by the server (`gw-nav-relay` binary) and
//! its clients (the visualizer, including the web build).
//!
//! - `GET /api/maps`: the MapDb rows as a JSON array of [`MapEntry`], with
//!   the zone def paths recorded for their map files.
//! - `GET /api/pathing/<mapfile_id>`: the map's stage-2 path chunk
//!   (`0x20000008` payload, `application/octet-stream`), from the relay's
//!   cache or downloaded and generated on demand. The revision it was
//!   generated from is in the [`FILE_ID_HEADER`] header. `?refresh=1`
//!   regenerates it from the current revision.
//! - `GET /api/annotations/<mapfile_id>`: [`MapAnnotations`] as JSON: the
//!   map file's mission points, portal props and Zones chunk, and the zone
//!   exits recorded for its maps.
//! - `GET /api/render/<mapfile_id>`: the map's baked top-down render (a
//!   [`crate::render::WorldRender`] file), from the cache or rendered on
//!   demand, for the revision of the cached pathing data. The revision is
//!   in the [`FILE_ID_HEADER`] header; `?refresh=1` renders the current
//!   revision again.
//! - Anything else is a static file of the web app.

use serde::{Deserialize, Serialize};

use crate::mapfile::mission::MissionPoint;
use crate::mapfile::zones::{Zone, ZoneDef};

pub const MAPS_PATH: &str = "/api/maps";
pub const PATHING_PATH: &str = "/api/pathing/";
pub const ANNOTATIONS_PATH: &str = "/api/annotations/";
pub const RENDER_PATH: &str = "/api/render/";
pub const FILE_ID_HEADER: &str = "X-Gw-File-Id";

/// Default address of a locally running relay.
pub const DEFAULT_RELAY: &str = "http://127.0.0.1:8080";

/// A map in the map list: a MapDb row, or a map file found in the asset
/// manifest that no row names (no mapid or name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapEntry {
    pub mapid: Option<u32>,
    pub name: Option<String>,
    pub mapfile: Option<u32>,
    /// The distinct zone def `.ini` paths recorded for the map file (empty
    /// until the map file has been loaded or scanned).
    #[serde(default)]
    pub zone_paths: Vec<String>,
}

/// A zone transition recorded in game (gwbs `zones.db`): walking into
/// (`x`, `y`) on `from_map` took the character to `to_map`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneExit {
    pub from_map: u32,
    pub to_map: Option<u32>,
    pub x: f32,
    pub y: f32,
    pub plane: u32,
    /// Times it was seen.
    pub hits: u32,
    /// Direction of travel into it, if recorded.
    pub dir: Option<[f32; 2]>,
}

/// A prop drawn with one of the portal models
/// ([`crate::mapfile::props::PORTAL_MODELS`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortalProp {
    /// Index in the map file's props list.
    pub prop: u32,
    /// Base file id of its model.
    pub model: u32,
    pub x: f32,
    pub y: f32,
    /// Yaw in 256ths of a turn.
    pub yaw: u8,
}

/// The map file's Zones chunk (stage 1): the procedurally populated areas
/// (grass, trees, rocks) and the defs they are populated from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneChunk {
    pub defs: Vec<ZoneDef>,
    pub zones: Vec<Zone>,
    /// The zones file-reference list (`0x11000003`): the defs' models in
    /// order, def by def.
    pub model_files: Vec<u32>,
}

impl ZoneChunk {
    /// The index in [`Self::model_files`] of each def's first model.
    pub fn model_starts(&self) -> Vec<usize> {
        self.defs
            .iter()
            .scan(0, |next, def| {
                let start = *next;
                *next += def.models.len();
                Some(start)
            })
            .collect()
    }
}

/// Points of interest on a map file, beside its pathing data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MapAnnotations {
    pub mission_points: Vec<MissionPoint>,
    pub zone_exits: Vec<ZoneExit>,
    #[serde(default)]
    pub portal_props: Vec<PortalProp>,
    #[serde(default)]
    pub zone_chunk: Option<ZoneChunk>,
    /// Map ids that use the map file (from MapDb).
    pub map_ids: Vec<u32>,
    /// Why some of it is missing, if it is.
    pub notes: Vec<String>,
}

pub fn annotations_url(base: &str, mapfile_id: u32) -> String {
    format!("{}{ANNOTATIONS_PATH}{mapfile_id}", base.trim_end_matches('/'))
}

/// Parse the map file id of an annotations request path.
pub fn parse_annotations_request(url: &str) -> Option<u32> {
    let rest = url.strip_prefix(ANNOTATIONS_PATH)?;
    rest.split('?').next()?.parse().ok()
}

/// The URL of a map's pathing data on the relay at `base`.
pub fn pathing_url(base: &str, mapfile_id: u32, refresh: bool) -> String {
    id_url(base, PATHING_PATH, mapfile_id, refresh)
}

/// The URL of a map's baked render on the relay at `base`.
pub fn render_url(base: &str, mapfile_id: u32, refresh: bool) -> String {
    id_url(base, RENDER_PATH, mapfile_id, refresh)
}

fn id_url(base: &str, path: &str, mapfile_id: u32, refresh: bool) -> String {
    let base = base.trim_end_matches('/');
    let query = if refresh { "?refresh=1" } else { "" };
    format!("{base}{path}{mapfile_id}{query}")
}

pub fn maps_url(base: &str) -> String {
    format!("{}{MAPS_PATH}", base.trim_end_matches('/'))
}

/// Parse the map file id (and refresh flag) of a pathing request path.
pub fn parse_pathing_request(url: &str) -> Option<(u32, bool)> {
    parse_id_request(url, PATHING_PATH)
}

/// Parse the map file id (and refresh flag) of a render request path.
pub fn parse_render_request(url: &str) -> Option<(u32, bool)> {
    parse_id_request(url, RENDER_PATH)
}

fn parse_id_request(url: &str, path: &str) -> Option<(u32, bool)> {
    let rest = url.strip_prefix(path)?;
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    let refresh = query.split('&').any(|kv| kv == "refresh=1" || kv == "refresh=true");
    Some((id.parse().ok()?, refresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathing_request_roundtrip() {
        let url = pathing_url("http://h:1/", 290943, true);
        assert_eq!(url, "http://h:1/api/pathing/290943?refresh=1");
        assert_eq!(parse_pathing_request("/api/pathing/290943?refresh=1"), Some((290943, true)));
        assert_eq!(parse_pathing_request("/api/pathing/12"), Some((12, false)));
        assert_eq!(parse_pathing_request("/api/pathing/x"), None);
        assert_eq!(parse_pathing_request("/api/maps"), None);
        assert_eq!(render_url("http://h:1", 7, false), "http://h:1/api/render/7");
        assert_eq!(parse_render_request("/api/render/7?refresh=1"), Some((7, true)));
        assert_eq!(parse_render_request("/api/pathing/7"), None);
    }

    #[test]
    fn annotations_request() {
        assert_eq!(annotations_url("http://h:1/", 290943), "http://h:1/api/annotations/290943");
        assert_eq!(parse_annotations_request("/api/annotations/290943"), Some(290943));
        assert_eq!(parse_annotations_request("/api/annotations/"), None);
        let a = MapAnnotations {
            zone_exits: vec![ZoneExit { from_map: 1, to_map: None, x: 1.0, y: 2.0, plane: 0, hits: 3, dir: Some([0.0, 1.0]) }],
            zone_chunk: Some(ZoneChunk {
                defs: vec![ZoneDef { id: 2, ini_path: "A\\Zones\\B.ini".into(), layers: vec![], models: vec![] }],
                zones: vec![Zone { def_id: 2, flags: 6, height_raw: 0x8000, vertices: vec![[1.0, 2.0], [3.0, 4.0]] }],
                model_files: vec![11830],
            }),
            ..Default::default()
        };
        assert_eq!(serde_json::from_str::<MapAnnotations>(&serde_json::to_string(&a).unwrap()).unwrap(), a);
    }

    #[test]
    fn map_entry_json() {
        for e in [
            MapEntry {
                mapid: Some(546),
                name: Some("Jaga Moraine".into()),
                mapfile: Some(290943),
                zone_paths: vec!["Chapter4\\Missions\\Mountain\\Ridge\\Zones\\MountainRidgeSnow.ini".into()],
            },
            MapEntry { mapid: None, name: None, mapfile: Some(13989), zone_paths: vec![] },
        ] {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<MapEntry>(&json).unwrap(), e);
        }
        // Lists from relays without zone paths.
        let old: MapEntry = serde_json::from_str(r#"{"mapid":1,"name":null,"mapfile":2}"#).unwrap();
        assert!(old.zone_paths.is_empty());
    }
}
