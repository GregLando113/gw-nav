//! Points of interest beside the pathing data ([`MapAnnotations`]): the map
//! file's mission points, portal props and Zones chunk, and the zone exits
//! recorded in game by gwbs (its `zones.db`, table `zone_exit`, keyed by map
//! id).

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params_from_iter};

use crate::api::{MapAnnotations, MapEntry, PortalProp, ZoneChunk, ZoneExit};
use crate::mapfile::props::{PORTAL_MODELS, PropsStrip};
use crate::mapfile::zones::ZonesStrip;
use crate::mapfile::{Ffna, MapFileError, mission, parse_file_refs};
use crate::{MapDb, MapDbError, PathingStore};

/// The exits recorded in `zones_db` from any of `maps`.
pub fn zone_exits(zones_db: &Path, maps: &[u32]) -> rusqlite::Result<Vec<ZoneExit>> {
    if maps.is_empty() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(zones_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let marks = vec!["?"; maps.len()].join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT from_map, to_map, x, y, plane, hits, dir_x, dir_y FROM zone_exit \
         WHERE from_map IN ({marks}) ORDER BY from_map, id"
    ))?;
    stmt.query_map(params_from_iter(maps), |row| {
        let dir: (Option<f64>, Option<f64>) = (row.get(6)?, row.get(7)?);
        Ok(ZoneExit {
            from_map: row.get(0)?,
            to_map: row.get(1)?,
            x: row.get::<_, f64>(2)? as f32,
            y: row.get::<_, f64>(3)? as f32,
            plane: row.get(4)?,
            hits: row.get(5)?,
            dir: match dir {
                (Some(x), Some(y)) => Some([x as f32, y as f32]),
                _ => None,
            },
        })
    })?
    .collect()
}

/// The props of a map file drawn with a portal model.
pub fn portal_props(file: &Ffna) -> Result<Vec<PortalProp>, MapFileError> {
    let props = PropsStrip::parse(file.chunk(0x1000_0004).ok_or(MapFileError::MissingChunk(0x1000_0004))?)?;
    let refs = match file.chunk(0x1100_0004) {
        Some(refs) => parse_file_refs(refs)?,
        None => Vec::new(),
    };
    Ok(props
        .props
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let model = *refs.get(p.model as usize)?;
            PORTAL_MODELS.contains(&model).then_some(PortalProp {
                prop: i as u32,
                model,
                x: p.position[0],
                y: p.position[1],
                yaw: p.rotation[2],
            })
        })
        .collect())
}

/// The map file's stage-1 Zones chunk, with its file-reference list.
pub fn zone_chunk(file: &Ffna) -> Result<ZoneChunk, MapFileError> {
    let strip = ZonesStrip::parse(file.chunk(0x1000_0003).ok_or(MapFileError::MissingChunk(0x1000_0003))?)?;
    let model_files = match file.chunk(0x1100_0003) {
        Some(refs) => parse_file_refs(refs)?,
        None => Vec::new(),
    };
    Ok(ZoneChunk { defs: strip.defs, zones: strip.zones, model_files })
}

/// Record in MapDb the zone def paths of revision `file_id` of map file
/// `mapfile_id` ([`MapDb::record_zone_defs`]). Returns whether anything
/// changed.
pub fn record_zone_paths(db: &Path, mapfile_id: u32, file_id: u32, chunk: &ZoneChunk) -> Result<bool, MapDbError> {
    let defs: Vec<(u32, &str)> = chunk.defs.iter().map(|d| (d.id, d.ini_path.as_str())).collect();
    MapDb::open(db)?.record_zone_defs(mapfile_id, file_id, &defs)
}

/// Everything known about map file `mapfile_id` (revision `file_id`) beyond
/// its pathing data. Missing parts are explained in `notes`.
pub fn annotations(
    store: &mut PathingStore,
    db: &Path,
    zones_db: Option<&Path>,
    mapfile_id: u32,
    file_id: u32,
) -> MapAnnotations {
    let mut out = MapAnnotations::default();
    match store.map_file(file_id).map_err(|e| e.to_string()) {
        Ok(data) => match Ffna::parse(&data) {
            Ok(file) => {
                let points = file
                    .chunk(0x1000_0007)
                    .ok_or_else(|| "no mission chunk".to_owned())
                    .and_then(|chunk| mission::parse(chunk).map_err(|e| e.to_string()));
                match points {
                    Ok(points) => out.mission_points = points,
                    Err(e) => out.notes.push(format!("Mission points: {e}")),
                }
                match portal_props(&file) {
                    Ok(props) => out.portal_props = props,
                    Err(e) => out.notes.push(format!("Portal props: {e}")),
                }
                match zone_chunk(&file) {
                    Ok(chunk) => out.zone_chunk = Some(chunk),
                    Err(e) => out.notes.push(format!("Zones chunk: {e}")),
                }
            }
            Err(e) => out.notes.push(format!("Map file {file_id}: {e}")),
        },
        Err(e) => out.notes.push(format!("Map file {file_id}: {e}")),
    }

    match MapDb::open(db).and_then(|db| db.all()) {
        Ok(zones) => {
            out.map_ids = zones.iter().filter(|z| z.mapfile == Some(mapfile_id)).map(|z| z.mapid).collect();
            // A map's outpost and explorable often share the file.
            out.map_ids.dedup();
        }
        Err(e) => out.notes.push(format!("MapDb {}: {e}", db.display())),
    }
    match zones_db {
        None => out.notes.push("Zone exits: no zones.db given (--zones-db)".into()),
        Some(_) if out.map_ids.is_empty() => {
            out.notes.push(format!("Zone exits: no MapDb row has map file {mapfile_id}"));
        }
        Some(path) => match zone_exits(path, &out.map_ids) {
            Ok(exits) => out.zone_exits = exits,
            Err(e) => out.notes.push(format!("Zone exits from {}: {e}", path.display())),
        },
    }
    out
}

/// The map list: the MapDb rows, then the map files found in the asset
/// manifest that no row names.
pub fn map_list(db: &Path) -> Result<Vec<MapEntry>, MapDbError> {
    let db = MapDb::open(db)?;
    let paths = db.zone_paths()?;
    let zone_paths = |mapfile: Option<u32>| mapfile.and_then(|f| paths.get(&f)).cloned().unwrap_or_default();
    let rows = db.all()?.into_iter().map(|z| MapEntry {
        mapid: Some(z.mapid),
        instance: Some(z.instance),
        name: z.name,
        zone_paths: zone_paths(z.mapfile),
        mapfile: z.mapfile,
    });
    let unlisted = db.unlisted_mapfiles()?.into_iter().map(|f| MapEntry {
        mapid: None,
        instance: None,
        name: None,
        mapfile: Some(f.mapfile),
        zone_paths: zone_paths(Some(f.mapfile)),
    });
    Ok(rows.chain(unlisted).collect())
}

/// Record in MapDb the map files of the asset manifest `store` has loaded,
/// if any ([`MapDb::record_manifest`]). Returns how many were new, or `None`
/// if no manifest is loaded or it was recorded before.
pub fn record_manifest_mapfiles(db: &Path, store: &PathingStore) -> Result<Option<usize>, MapDbError> {
    let Some((id, manifest)) = store.loaded_manifest() else { return Ok(None) };
    MapDb::open(db)?.record_manifest(id, &manifest.map_file_candidates())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eye of the North's five portals (model 247212), among them the one at
    /// its exit to map 499.
    #[test]
    fn finds_portal_props() {
        let Some(f) = crate::pathgen::testing::Fixture::all().find(|f| f.pair.base_id == 288299) else { return };
        let strip = f.pair.strip().unwrap();
        let props = portal_props(&Ffna::parse(&strip).unwrap()).unwrap();
        assert_eq!(props.len(), 5);
        assert!(props.iter().all(|p| p.model == 247212));
        assert!(props.iter().any(|p| (p.x, p.y) == (787.0, 1053.0)));
    }

    #[test]
    fn reads_zone_chunk() {
        let Some(strip) = crate::mapfile::testdata::MapPair::all().find(|p| p.base_id == 290943).and_then(|p| p.strip())
        else {
            return;
        };
        let chunk = zone_chunk(&Ffna::parse(&strip).unwrap()).unwrap();
        assert_eq!((chunk.defs.len(), chunk.zones.len(), chunk.model_files.len()), (6, 8, 123));
        assert_eq!(chunk.model_starts(), [0, 21, 68, 70, 95, 99]);

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("maps.db");
        assert!(record_zone_paths(&db, 290943, 381071, &chunk).unwrap());
        assert!(!record_zone_paths(&db, 290943, 381071, &chunk).unwrap());
        let zone = crate::MapZone {
            mapid: 546,
            instance: crate::Instance::Explorable,
            name: None,
            mapfile: Some(290943),
            unknown: None,
        };
        MapDb::open(&db).unwrap().upsert(&zone).unwrap();
        let list = map_list(&db).unwrap();
        assert_eq!(list[0].zone_paths.len(), 6);
        assert!(list[0].zone_paths.iter().all(|p| p.starts_with(r"Chapter4\Missions\Mountain\Ridge\Zones\")));
    }

    #[test]
    fn reads_zone_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zones.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE zone_exit (id INTEGER PRIMARY KEY, from_map INTEGER NOT NULL, x REAL NOT NULL,
                 y REAL NOT NULL, plane INTEGER NOT NULL, to_map INTEGER, trapezoid INTEGER,
                 hits INTEGER NOT NULL DEFAULT 1, dir_x REAL, dir_y REAL);
             INSERT INTO zone_exit (from_map, x, y, plane, to_map, hits, dir_x, dir_y)
                 VALUES (546, -13212.5, -24238, 0, 643, 2, 0.6, -0.8), (546, 1, 2, 3, NULL, 1, NULL, NULL),
                        (642, 5, 6, 0, 499, 1, NULL, NULL);",
        )
        .unwrap();
        drop(conn);
        let exits = zone_exits(&path, &[546, 7]).unwrap();
        assert_eq!(
            exits,
            vec![
                ZoneExit { from_map: 546, to_map: Some(643), x: -13212.5, y: -24238.0, plane: 0, hits: 2, dir: Some([0.6, -0.8]) },
                ZoneExit { from_map: 546, to_map: None, x: 1.0, y: 2.0, plane: 3, hits: 1, dir: None },
            ]
        );
        assert_eq!(zone_exits(&path, &[]).unwrap(), vec![]);
        assert!(zone_exits(&dir.path().join("missing.db"), &[1]).is_err());
    }
}
