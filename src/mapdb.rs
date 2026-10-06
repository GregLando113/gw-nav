//! MapDb: sqlite table of map zones and their associated mapfile ids.
//!
//! `map_zones` has a row per map id and [`Instance`]: a map id can load one
//! map file as an outpost and another as an explorable area (most use the
//! same file for both). The schema matches the table written by GWBS
//! `maploadlog.lua`, so rows found by other tools can be merged in with
//! [`MapDb::import_from`]. Databases from before the instance column are
//! rebuilt on open, their rows kept as outposts.
//!
//! A second table, `manifest_mapfiles`, holds the map files found in the
//! fileserver's asset manifest ([`MapDb::record_manifest`]). The manifest
//! has no map ids, so they are kept apart from `map_zones`.
//!
//! A third, `mapfile_zone_defs`, holds the zone def `.ini` paths found in
//! each map file's Zones chunk ([`MapDb::record_zone_defs`]). They follow
//! the developers' folder tree, which helps place unidentified map files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

pub use crate::api::Instance;
use crate::fileconn::MapFileCandidate;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS map_zones (
    mapid INTEGER NOT NULL,
    instance TEXT NOT NULL CHECK (instance IN ('outpost', 'explorable')),
    name TEXT,
    mapfile INTEGER,
    unknown INTEGER,
    PRIMARY KEY (mapid, instance)
);
CREATE TABLE IF NOT EXISTS manifest_mapfiles (
    mapfile INTEGER PRIMARY KEY,
    revision INTEGER NOT NULL,
    manifest INTEGER NOT NULL,
    dependencies INTEGER NOT NULL,
    is_map INTEGER
);
CREATE TABLE IF NOT EXISTS mapfile_zone_defs (
    mapfile INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    def_id INTEGER NOT NULL,
    ini_path TEXT NOT NULL,
    PRIMARY KEY (mapfile, def_id)
);";

/// Rebuilds a `map_zones` table from before the instance column; its rows
/// become outposts.
const ADD_INSTANCE: &str = "
ALTER TABLE map_zones RENAME TO map_zones_without_instance;
CREATE TABLE map_zones (
    mapid INTEGER NOT NULL,
    instance TEXT NOT NULL CHECK (instance IN ('outpost', 'explorable')),
    name TEXT,
    mapfile INTEGER,
    unknown INTEGER,
    PRIMARY KEY (mapid, instance)
);
INSERT INTO map_zones (mapid, instance, name, mapfile, unknown)
    SELECT mapid, 'outpost', name, mapfile, unknown FROM map_zones_without_instance;
DROP TABLE map_zones_without_instance;";

const SELECT_COLUMNS: &str = "SELECT mapid, instance, name, mapfile, unknown FROM map_zones";
/// Outposts before explorables.
const ORDER: &str = "ORDER BY mapid, instance DESC";

/// One row of the `map_zones` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapZone {
    pub mapid: u32,
    /// The instance `mapfile` is loaded for.
    pub instance: Instance,
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
    /// Rows from a table without the instance column whose map file no
    /// local row of their map id has. Not applied: it isn't known which
    /// instance the file is for.
    pub unplaced: Vec<UnplacedZone>,
}

/// A `map_zones` row from before the instance column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnplacedZone {
    pub mapid: u32,
    pub name: Option<String>,
    pub mapfile: Option<u32>,
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

    fn init(mut conn: Connection) -> Result<Self> {
        if has_table(&conn, "map_zones")? && !has_column(&conn, "map_zones", "instance")? {
            let tx = conn.transaction()?;
            tx.execute_batch(ADD_INSTANCE)?;
            tx.commit()?;
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// All rows ordered by mapid, outposts first.
    pub fn all(&self) -> Result<Vec<MapZone>> {
        query_zones(&self.conn, &format!("{SELECT_COLUMNS} {ORDER}"), [])
    }

    /// The rows of map `mapid`, outpost first.
    pub fn get(&self, mapid: u32) -> Result<Vec<MapZone>> {
        query_zones(&self.conn, &format!("{SELECT_COLUMNS} WHERE mapid = ?1 {ORDER}"), [mapid])
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
                 {ORDER}"
            ),
            params![pattern, number],
        )
    }

    /// Insert the row, replacing any existing row with the same mapid and
    /// instance.
    pub fn upsert(&self, zone: &MapZone) -> Result<()> {
        upsert_zone(&self.conn, zone)
    }

    /// Set the mapfile of the `instance` of map `mapid` (`None` clears it),
    /// keeping the rest of its row. A new row takes its name from the map's
    /// other instance, if it has one.
    pub fn set_mapfile(&self, mapid: u32, instance: Instance, mapfile: Option<u32>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO map_zones (mapid, instance, name, mapfile)
                 VALUES (?1, ?2, (SELECT name FROM map_zones WHERE mapid = ?1 LIMIT 1), ?3)
             ON CONFLICT(mapid, instance) DO UPDATE SET mapfile = excluded.mapfile",
            params![mapid, instance, mapfile],
        )?;
        Ok(())
    }

    /// Merge all rows from the `map_zones` table of another database.
    /// New rows are always inserted. Incoming rows overwrite existing local
    /// rows only if they carry a mapfile (non-empty, non-zero). A table
    /// without the instance column (GWBS `maploadlog.lua` before it logged
    /// instances) adds nothing: its rows are unchanged if their map id has
    /// a row with that mapfile, and [`ImportReport::unplaced`] otherwise.
    /// The merge runs in a single transaction, so on error the local
    /// database is unchanged.
    pub fn import_from(&mut self, path: impl AsRef<Path>) -> Result<ImportReport> {
        let path = path.as_ref();
        let src = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if !has_table(&src, "map_zones")? {
            return Err(MapDbError::MissingTable {
                path: path.to_path_buf(),
            });
        }
        if !has_column(&src, "map_zones", "instance")? {
            return self.import_without_instance(&src);
        }
        let incoming = query_zones(&src, &format!("{SELECT_COLUMNS} {ORDER}"), [])?;

        let tx = self.conn.transaction()?;
        let mut report = ImportReport::default();
        for zone in incoming {
            match get_zone(&tx, zone.mapid, zone.instance)? {
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

    fn import_without_instance(&self, src: &Connection) -> Result<ImportReport> {
        let mut stmt = src.prepare("SELECT mapid, name, mapfile FROM map_zones ORDER BY mapid")?;
        let incoming = stmt
            .query_map([], |row| Ok(UnplacedZone { mapid: row.get(0)?, name: row.get(1)?, mapfile: row.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut report = ImportReport::default();
        for zone in incoming {
            let known: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM map_zones WHERE mapid = ?1 AND mapfile IS ?2)",
                params![zone.mapid, zone.mapfile],
                |row| row.get(0),
            )?;
            if known {
                report.unchanged += 1;
            } else {
                report.unplaced.push(zone);
            }
        }
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

    /// Record the zone defs (`(def id, .ini path)`) of revision `revision`
    /// of map file `mapfile`, replacing what was recorded for it. Returns
    /// whether anything changed.
    pub fn record_zone_defs(&mut self, mapfile: u32, revision: u32, defs: &[(u32, &str)]) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let old: Vec<(u32, u32, String)> = {
            let mut stmt =
                tx.prepare("SELECT revision, def_id, ini_path FROM mapfile_zone_defs WHERE mapfile = ?1 ORDER BY def_id")?;
            stmt.query_map([mapfile], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut new: Vec<(u32, u32, String)> = defs.iter().map(|&(id, path)| (revision, id, path.to_owned())).collect();
        new.sort_by_key(|&(_, id, _)| id);
        new.dedup_by_key(|&mut (_, id, _)| id);
        if old == new {
            return Ok(false);
        }
        tx.execute("DELETE FROM mapfile_zone_defs WHERE mapfile = ?1", [mapfile])?;
        for (revision, id, path) in &new {
            tx.execute(
                "INSERT INTO mapfile_zone_defs (mapfile, revision, def_id, ini_path) VALUES (?1, ?2, ?3, ?4)",
                params![mapfile, revision, id, path],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The distinct zone def paths recorded for each map file, sorted.
    pub fn zone_paths(&self) -> Result<HashMap<u32, Vec<String>>> {
        let mut stmt =
            self.conn.prepare("SELECT DISTINCT mapfile, ini_path FROM mapfile_zone_defs ORDER BY mapfile, ini_path")?;
        let mut out: HashMap<u32, Vec<String>> = HashMap::new();
        for row in stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))? {
            let (mapfile, path) = row?;
            out.entry(mapfile).or_default().push(path);
        }
        Ok(out)
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

impl ToSql for Instance {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Instance {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e: String| FromSqlError::Other(e.into()))
    }
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [table, column],
        |row| row.get(0),
    )?)
}

fn row_to_zone(row: &Row) -> rusqlite::Result<MapZone> {
    Ok(MapZone {
        mapid: row.get(0)?,
        instance: row.get(1)?,
        name: row.get(2)?,
        mapfile: row.get(3)?,
        unknown: row.get(4)?,
    })
}

fn query_zones(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<MapZone>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, row_to_zone)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn get_zone(conn: &Connection, mapid: u32, instance: Instance) -> Result<Option<MapZone>> {
    Ok(conn
        .query_row(
            &format!("{SELECT_COLUMNS} WHERE mapid = ?1 AND instance = ?2"),
            params![mapid, instance],
            row_to_zone,
        )
        .optional()?)
}

fn upsert_zone(conn: &Connection, zone: &MapZone) -> Result<()> {
    conn.execute(
        "INSERT INTO map_zones (mapid, instance, name, mapfile, unknown) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(mapid, instance) DO UPDATE SET
             name = excluded.name, mapfile = excluded.mapfile, unknown = excluded.unknown",
        params![zone.mapid, zone.instance, zone.name, zone.mapfile, zone.unknown],
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
            instance: Instance::Outpost,
            name: Some(name.to_string()),
            mapfile: Some(mapfile),
            unknown: Some(0),
        }
    }

    fn explorable(mapid: u32, name: &str, mapfile: u32) -> MapZone {
        MapZone { instance: Instance::Explorable, ..zone(mapid, name, mapfile) }
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
    fn adds_instance_column_to_old_table() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE map_zones (mapid INTEGER PRIMARY KEY, name TEXT, mapfile INTEGER, unknown INTEGER);
                 INSERT INTO map_zones VALUES (109, 'The Amnoon Oasis', 290943, 0);",
            )
            .unwrap();
        let db = MapDb::open(&path).unwrap();
        assert_eq!(db.all().unwrap(), vec![zone(109, "The Amnoon Oasis", 290943)]);
        db.upsert(&explorable(109, "The Amnoon Oasis", 1)).unwrap();
        assert_eq!(db.get(109).unwrap().len(), 2);
    }

    #[test]
    fn upsert_get_all_round_trip() {
        let db = sample_db();
        let nulls = MapZone {
            mapid: 5,
            instance: Instance::Explorable,
            name: None,
            mapfile: None,
            unknown: None,
        };
        db.upsert(&nulls).unwrap();
        assert_eq!(db.get(5).unwrap(), vec![nulls.clone()]);
        assert_eq!(db.get(999).unwrap(), vec![]);

        db.upsert(&zone(109, "Renamed", 1)).unwrap();
        assert_eq!(db.get(109).unwrap(), vec![zone(109, "Renamed", 1)]);

        // The instances of a map are separate rows, outpost first.
        db.upsert(&explorable(232, "Shadow's Passage", 2)).unwrap();
        assert_eq!(
            db.get(232).unwrap(),
            vec![zone(232, "Shadow's Passage", 290923), explorable(232, "Shadow's Passage", 2)]
        );

        let ids: Vec<u32> = db.all().unwrap().iter().map(|z| z.mapid).collect();
        assert_eq!(ids, vec![5, 109, 232, 232]);
    }

    #[test]
    fn set_mapfile_keeps_row() {
        let db = sample_db();
        db.set_mapfile(109, Instance::Outpost, Some(500)).unwrap();
        assert_eq!(db.get(109).unwrap(), vec![zone(109, "The Amnoon Oasis", 500)]);
        db.set_mapfile(109, Instance::Outpost, None).unwrap();
        assert_eq!(db.get(109).unwrap()[0].mapfile, None);
        // A map's other instance gets a row named after the first.
        db.set_mapfile(232, Instance::Explorable, Some(700)).unwrap();
        let other = MapZone { unknown: None, ..explorable(232, "Shadow's Passage", 700) };
        assert_eq!(db.get(232).unwrap(), vec![zone(232, "Shadow's Passage", 290923), other]);
        db.set_mapfile(7, Instance::Outpost, Some(600)).unwrap();
        let new = MapZone { mapid: 7, instance: Instance::Outpost, name: None, mapfile: Some(600), unknown: None };
        assert_eq!(db.get(7).unwrap(), vec![new]);
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
            other.upsert(&explorable(232, "Shadow's Passage", 333)).unwrap(); // inserted
            other.upsert(&zone(300, "New Map", 222)).unwrap(); // inserted
        }

        let mut db = sample_db();
        let report = db.import_from(&other_path).unwrap();
        assert_eq!(report.inserted, vec![explorable(232, "Shadow's Passage", 333), zone(300, "New Map", 222)]);
        assert_eq!(
            report.updated,
            vec![(
                zone(232, "Shadow's Passage", 290923),
                zone(232, "Shadow's Passage", 111)
            )]
        );
        assert_eq!(report.unchanged, 1);
        assert_eq!(
            db.get(232).unwrap(),
            vec![zone(232, "Shadow's Passage", 111), explorable(232, "Shadow's Passage", 333)]
        );
        assert_eq!(db.all().unwrap().len(), 4);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn import_without_instance_only_reports() {
        let dir = TempDir::new().unwrap();
        let other_path = dir.path().join("maploadlog.db");
        Connection::open(&other_path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE map_zones (mapid INTEGER PRIMARY KEY, name TEXT, mapfile INTEGER, unknown INTEGER);
                 INSERT INTO map_zones VALUES (109, 'The Amnoon Oasis', 290943, 0), (232, 'Shadow''s Passage', 111, 0);",
            )
            .unwrap();
        let mut db = sample_db();
        let before = db.all().unwrap();
        let report = db.import_from(&other_path).unwrap();
        assert_eq!(report.unchanged, 1);
        assert_eq!(
            report.unplaced,
            vec![UnplacedZone { mapid: 232, name: Some("Shadow's Passage".into()), mapfile: Some(111) }]
        );
        assert!(report.inserted.is_empty() && report.updated.is_empty());
        assert_eq!(db.all().unwrap(), before);
    }

    #[test]
    fn import_skips_overwrite_without_mapfile() {
        let dir = TempDir::new().unwrap();
        let other_path = dir.path().join("other.db");
        let zero = zone(109, "Zero Mapfile", 0);
        let null = MapZone {
            mapid: 232,
            instance: Instance::Outpost,
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
        assert_eq!(db.get(109).unwrap(), vec![zone(109, "The Amnoon Oasis", 290943)]);
        assert_eq!(db.get(232).unwrap(), vec![zone(232, "Shadow's Passage", 290923)]);
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
    fn records_zone_defs() {
        let mut db = sample_db();
        let a = r"Chapter3\Missions\Nightmare\Town\Zones\NightmareTownGrass.ini";
        let b = r"Chapter3\Missions\Nightmare\Town\Zones\NightmareTownCreepy.ini";
        assert!(db.record_zone_defs(214315, 380854, &[(2, a), (1, b)]).unwrap());
        assert!(!db.record_zone_defs(214315, 380854, &[(1, b), (2, a)]).unwrap());
        assert!(db.record_zone_defs(500, 501, &[(1, a)]).unwrap());
        let paths = db.zone_paths().unwrap();
        assert_eq!(paths[&214315], [b, a]);
        assert_eq!(paths[&500], [a]);

        // A new revision replaces the old rows.
        assert!(db.record_zone_defs(214315, 390000, &[(1, a)]).unwrap());
        assert_eq!(db.zone_paths().unwrap()[&214315], [a]);
        assert!(db.record_zone_defs(500, 501, &[]).unwrap());
        assert!(!db.zone_paths().unwrap().contains_key(&500));
    }

    #[test]
    fn reads_repo_fixture() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mapfiles.db");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/mapfiles.db"), &path).unwrap();
        let db = MapDb::open(&path).unwrap();
        assert_eq!(db.get(109).unwrap(), vec![zone(109, "The Amnoon Oasis", 52918)]);
        assert_eq!(db.get(642).unwrap(), vec![zone(642, "Eye of the North", 288299)]);
        // A mission: its outpost's file, and a row for the mission's.
        let wall = db.get(28).unwrap();
        assert_eq!(wall.iter().map(|z| z.instance).collect::<Vec<_>>(), Instance::ALL);
        assert!(wall[0].has_mapfile());
        // An explorable area.
        assert_eq!(db.get(13).unwrap()[0].instance, Instance::Explorable);
    }
}
