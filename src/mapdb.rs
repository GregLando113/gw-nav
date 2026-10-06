//! MapDb: sqlite table of map zones and their associated mapfile ids.
//!
//! The schema matches the `map_zones` table written by GWBS `maploadlog.lua`,
//! so rows found by other tools can be merged in with [`MapDb::import_from`].
//!
//! A second table, `manifest_mapfiles`, holds the map files found in the
//! fileserver's asset manifest ([`MapDb::record_manifest`]). The manifest
//! has no map ids, so they are kept apart from `map_zones`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

use crate::fileconn::MapFileCandidate;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS map_zones (
    mapid INTEGER PRIMARY KEY,
    name TEXT,
    mapfile INTEGER,
    unknown INTEGER
);
CREATE TABLE IF NOT EXISTS manifest_mapfiles (
    mapfile INTEGER PRIMARY KEY,
    revision INTEGER NOT NULL,
    manifest INTEGER NOT NULL,
    dependencies INTEGER NOT NULL,
    is_map INTEGER
);";

const SELECT_COLUMNS: &str = "SELECT mapid, name, mapfile, unknown FROM map_zones";

/// One row of the `map_zones` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapZone {
    pub mapid: u32,
    pub name: Option<String>,
    /// File id of the map blob on the GW fileserver.
    pub mapfile: Option<u32>,
    pub unknown: Option<i64>,
}

impl MapZone {
    /// True if the mapfile id is known (not NULL and not 0).
    pub fn has_mapfile(&self) -> bool {
        self.mapfile.is_some_and(|id| id != 0)
    }
}

/// One row of the `manifest_mapfiles` table: a file in the asset manifest
/// that looks like a map file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestMapFile {
    /// Base file id, as in `map_zones.mapfile`.
    pub mapfile: u32,
    /// Current file id when last seen.
    pub revision: u32,
    /// File id of the asset manifest it was last seen in.
    pub manifest: u32,
    pub dependencies: u32,
    /// Whether the file was checked to be a map file; `None` if unchecked.
    pub is_map: Option<bool>,
}

/// Result of merging another database into this one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Rows whose mapid did not exist locally.
    pub inserted: Vec<MapZone>,
    /// Rows that replaced a differing local row, as `(old, new)`.
    pub updated: Vec<(MapZone, MapZone)>,
    /// Number of rows identical to the local row.
    pub unchanged: usize,
    /// Incoming rows that differed from the local row but were not applied
    /// because their mapfile was empty or 0.
    pub skipped: Vec<MapZone>,
}

#[derive(thiserror::Error, Debug)]
pub enum MapDbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{} has no map_zones table", path.display())]
    MissingTable { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, MapDbError>;

pub struct MapDb {
    conn: Connection,
}

impl MapDb {
    /// Open (or create) the database at `path`, creating the table if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// All rows ordered by mapid.
    pub fn all(&self) -> Result<Vec<MapZone>> {
        query_zones(&self.conn, &format!("{SELECT_COLUMNS} ORDER BY mapid"), [])
    }

    pub fn get(&self, mapid: u32) -> Result<Option<MapZone>> {
        get_zone(&self.conn, mapid)
    }

    /// Case-insensitive substring match on name. If `query` is a number it
    /// also matches rows whose mapid or mapfile equal it.
    pub fn search(&self, query: &str) -> Result<Vec<MapZone>> {
        let query = query.trim();
        let pattern = format!("%{}%", escape_like(query));
        let number = query.parse::<i64>().ok();
        query_zones(
            &self.conn,
            &format!(
                "{SELECT_COLUMNS}
                 WHERE name LIKE ?1 ESCAPE '\\' OR mapid = ?2 OR mapfile = ?2
                 ORDER BY mapid"
            ),
            params![pattern, number],
        )
    }

    /// Insert the row, replacing any existing row with the same mapid.
    pub fn upsert(&self, zone: &MapZone) -> Result<()> {
        upsert_zone(&self.conn, zone)
    }

    /// Merge all rows from the `map_zones` table of another database.
    /// New mapids are always inserted. Incoming rows overwrite existing local
    /// rows only if they carry a mapfile (non-empty, non-zero). The merge runs
    /// in a single transaction, so on error the local database is unchanged.
    pub fn import_from(&mut self, path: impl AsRef<Path>) -> Result<ImportReport> {
        let path = path.as_ref();
        let src = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let has_table: bool = src.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'map_zones')",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Err(MapDbError::MissingTable {
                path: path.to_path_buf(),
            });
        }
        let incoming = query_zones(&src, &format!("{SELECT_COLUMNS} ORDER BY mapid"), [])?;

        let tx = self.conn.transaction()?;
        let mut report = ImportReport::default();
        for zone in incoming {
            match get_zone(&tx, zone.mapid)? {
                None => {
                    upsert_zone(&tx, &zone)?;
                    report.inserted.push(zone);
                }
                Some(old) if old == zone => report.unchanged += 1,
                Some(_) if !zone.has_mapfile() => report.skipped.push(zone),
                Some(old) => {
                    upsert_zone(&tx, &zone)?;
                    report.updated.push((old, zone));
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Record the map file candidates of asset manifest `manifest`
    /// ([`crate::AssetManifest::map_file_candidates`]). Known files get the
    /// new revision and keep whether they were checked to be maps. Returns
    /// how many new files no `map_zones` row names, or `None` if this
    /// manifest was recorded before (it only changes with game updates).
    pub fn record_manifest(&mut self, manifest: u32, candidates: &[MapFileCandidate]) -> Result<Option<usize>> {
        let tx = self.conn.transaction()?;
        let seen: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM manifest_mapfiles WHERE manifest = ?1)", [manifest], |row| {
                row.get(0)
            })?;
        if seen {
            return Ok(None);
        }
        let mut new = 0;
        for c in candidates {
            let known: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM manifest_mapfiles WHERE mapfile = ?1)
                     OR EXISTS(SELECT 1 FROM map_zones WHERE mapfile = ?1)",
                [c.base_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO manifest_mapfiles (mapfile, revision, manifest, dependencies) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(mapfile) DO UPDATE SET
                     revision = excluded.revision, manifest = excluded.manifest,
                     dependencies = excluded.dependencies",
                params![c.base_id, c.current_id, manifest, c.dependencies as u32],
            )?;
            new += usize::from(!known);
        }
        tx.commit()?;
        Ok(Some(new))
    }

    /// Record whether a `manifest_mapfiles` file is a map file.
    pub fn set_is_map(&self, mapfile: u32, is_map: bool) -> Result<()> {
        self.conn.execute("UPDATE manifest_mapfiles SET is_map = ?2 WHERE mapfile = ?1", params![mapfile, is_map])?;
        Ok(())
    }

    /// All `manifest_mapfiles` rows, by mapfile.
    pub fn manifest_mapfiles(&self) -> Result<Vec<ManifestMapFile>> {
        query_manifest_mapfiles(&self.conn, "")
    }

    /// The manifest's map files that no `map_zones` row names, less those
    /// checked not to be maps.
    pub fn unlisted_mapfiles(&self) -> Result<Vec<ManifestMapFile>> {
        query_manifest_mapfiles(
            &self.conn,
            "WHERE (is_map IS NULL OR is_map != 0)
               AND mapfile NOT IN (SELECT mapfile FROM map_zones WHERE mapfile IS NOT NULL)",
        )
    }
}

fn query_manifest_mapfiles(conn: &Connection, filter: &str) -> Result<Vec<ManifestMapFile>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT mapfile, revision, manifest, dependencies, is_map FROM manifest_mapfiles {filter} ORDER BY mapfile"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok(ManifestMapFile {
            mapfile: row.get(0)?,
            revision: row.get(1)?,
            manifest: row.get(2)?,
            dependencies: row.get(3)?,
            is_map: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn row_to_zone(row: &Row) -> rusqlite::Result<MapZone> {
    Ok(MapZone {
        mapid: row.get(0)?,
        name: row.get(1)?,
        mapfile: row.get(2)?,
        unknown: row.get(3)?,
    })
}

fn query_zones(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<MapZone>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, row_to_zone)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn get_zone(conn: &Connection, mapid: u32) -> Result<Option<MapZone>> {
    Ok(conn
        .query_row(
            &format!("{SELECT_COLUMNS} WHERE mapid = ?1"),
            [mapid],
            row_to_zone,
        )
        .optional()?)
}

fn upsert_zone(conn: &Connection, zone: &MapZone) -> Result<()> {
    conn.execute(
        "INSERT INTO map_zones (mapid, name, mapfile, unknown) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(mapid) DO UPDATE SET
             name = excluded.name, mapfile = excluded.mapfile, unknown = excluded.unknown",
        params![zone.mapid, zone.name, zone.mapfile, zone.unknown],
    )?;
    Ok(())
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn zone(mapid: u32, name: &str, mapfile: u32) -> MapZone {
        MapZone {
            mapid,
            name: Some(name.to_string()),
            mapfile: Some(mapfile),
            unknown: Some(0),
        }
    }

    fn sample_db() -> MapDb {
        let db = MapDb::open_in_memory().unwrap();
        db.upsert(&zone(109, "The Amnoon Oasis", 290943)).unwrap();
        db.upsert(&zone(232, "Shadow's Passage", 290923)).unwrap();
        db
    }

    #[test]
    fn open_creates_table_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.db");
        {
            let db = MapDb::open(&path).unwrap();
            assert!(db.all().unwrap().is_empty());
            db.upsert(&zone(1, "A", 10)).unwrap();
        }
        let db = MapDb::open(&path).unwrap();
        assert_eq!(db.all().unwrap(), vec![zone(1, "A", 10)]);
    }

    #[test]
    fn upsert_get_all_round_trip() {
        let db = sample_db();
        let nulls = MapZone {
            mapid: 5,
            name: None,
            mapfile: None,
            unknown: None,
        };
        db.upsert(&nulls).unwrap();
        assert_eq!(db.get(5).unwrap(), Some(nulls.clone()));
        assert_eq!(db.get(999).unwrap(), None);

        db.upsert(&zone(109, "Renamed", 1)).unwrap();
        assert_eq!(db.get(109).unwrap(), Some(zone(109, "Renamed", 1)));

        let ids: Vec<u32> = db.all().unwrap().iter().map(|z| z.mapid).collect();
        assert_eq!(ids, vec![5, 109, 232]);
    }

    #[test]
    fn search_by_name_and_number() {
        let db = sample_db();
        let ids = |q: &str| -> Vec<u32> {
            db.search(q).unwrap().iter().map(|z| z.mapid).collect()
        };
        assert_eq!(ids("amnoon"), vec![109]);
        assert_eq!(ids("PASSAGE"), vec![232]);
        assert_eq!(ids("109"), vec![109]);
        assert_eq!(ids("290923"), vec![232]);
        assert_eq!(ids(""), vec![109, 232]);
        assert_eq!(ids("%"), Vec::<u32>::new());
        assert_eq!(ids("nothing"), Vec::<u32>::new());
    }

    #[test]
    fn import_inserts_and_overwrites() {
        let dir = TempDir::new().unwrap();
        let other_path = dir.path().join("other.db");
        {
            let other = MapDb::open(&other_path).unwrap();
            other.upsert(&zone(109, "The Amnoon Oasis", 290943)).unwrap(); // unchanged
            other.upsert(&zone(232, "Shadow's Passage", 111)).unwrap(); // updated
            other.upsert(&zone(300, "New Map", 222)).unwrap(); // inserted
        }

        let mut db = sample_db();
        let report = db.import_from(&other_path).unwrap();
        assert_eq!(report.inserted, vec![zone(300, "New Map", 222)]);
        assert_eq!(
            report.updated,
            vec![(
                zone(232, "Shadow's Passage", 290923),
                zone(232, "Shadow's Passage", 111)
            )]
        );
        assert_eq!(report.unchanged, 1);
        assert_eq!(db.get(232).unwrap(), Some(zone(232, "Shadow's Passage", 111)));
        assert_eq!(db.all().unwrap().len(), 3);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn import_skips_overwrite_without_mapfile() {
        let dir = TempDir::new().unwrap();
        let other_path = dir.path().join("other.db");
        let zero = zone(109, "Zero Mapfile", 0);
        let null = MapZone {
            mapid: 232,
            name: Some("Null Mapfile".into()),
            mapfile: None,
            unknown: None,
        };
        let new_without_mapfile = zone(300, "New Map", 0);
        {
            let other = MapDb::open(&other_path).unwrap();
            other.upsert(&zero).unwrap();
            other.upsert(&null).unwrap();
            other.upsert(&new_without_mapfile).unwrap();
        }

        let mut db = sample_db();
        let report = db.import_from(&other_path).unwrap();
        assert_eq!(report.skipped, vec![zero, null]);
        assert!(report.updated.is_empty());
        // New mapids are still inserted even without a mapfile.
        assert_eq!(report.inserted, vec![new_without_mapfile]);
        assert_eq!(db.get(109).unwrap(), Some(zone(109, "The Amnoon Oasis", 290943)));
        assert_eq!(db.get(232).unwrap(), Some(zone(232, "Shadow's Passage", 290923)));
    }

    #[test]
    fn import_without_table_errors_and_leaves_db_unchanged() {
        let dir = TempDir::new().unwrap();
        let other_path = dir.path().join("empty.db");
        Connection::open(&other_path)
            .unwrap()
            .execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();

        let mut db = sample_db();
        let before = db.all().unwrap();
        let err = db.import_from(&other_path).unwrap_err();
        assert!(matches!(err, MapDbError::MissingTable { .. }));
        assert_eq!(db.all().unwrap(), before);
    }

    #[test]
    fn records_manifest_map_files() {
        let candidate = |base_id, current_id| MapFileCandidate { base_id, current_id, dependencies: 80 };
        let mut db = sample_db();
        // 290943 is in map_zones already.
        let first = [candidate(290943, 381071), candidate(500, 501), candidate(600, 601)];
        assert_eq!(db.record_manifest(10, &first).unwrap(), Some(2));
        assert_eq!(db.record_manifest(10, &first).unwrap(), None);
        let mapfiles = |rows: Vec<ManifestMapFile>| rows.iter().map(|r| r.mapfile).collect::<Vec<_>>();
        assert_eq!(mapfiles(db.unlisted_mapfiles().unwrap()), [500, 600]);

        // A file checked not to be a map leaves the list, and stays checked
        // when a later manifest lists it with a new revision.
        db.set_is_map(600, false).unwrap();
        db.set_is_map(500, true).unwrap();
        assert_eq!(db.record_manifest(11, &[candidate(500, 502), candidate(600, 602), candidate(700, 701)]).unwrap(), Some(1));
        assert_eq!(mapfiles(db.unlisted_mapfiles().unwrap()), [500, 700]);
        let rows = db.manifest_mapfiles().unwrap();
        let row = |id| rows.iter().find(|r| r.mapfile == id).unwrap();
        assert_eq!((row(500).revision, row(500).manifest, row(500).is_map), (502, 11, Some(true)));
        assert_eq!(row(600).is_map, Some(false));
        assert_eq!(row(700).is_map, None);

        // Once GWBS logs a map with that file, it is listed under its mapid.
        db.upsert(&zone(900, "Found", 700)).unwrap();
        assert_eq!(mapfiles(db.unlisted_mapfiles().unwrap()), [500]);
    }

    #[test]
    fn reads_repo_fixture() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mapfiles.db");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/mapfiles.db"), &path).unwrap();
        let db = MapDb::open(&path).unwrap();
        assert_eq!(db.get(109).unwrap(), Some(zone(109, "The Amnoon Oasis", 52918)));
        assert_eq!(db.get(642).unwrap(), Some(zone(642, "Eye of the North", 288299)));
    }
}
