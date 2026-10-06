mod progress;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gw_nav::mapfile::ffna::TYPE_MAP;
use gw_nav::mapfile::zones::{self, ZonesStrip};
use gw_nav::mapfile::{ChunkId, Ffna, parse_file_refs};
use gw_nav::pathing::Progress;
use gw_nav::fileconn::{ConnectionPool, Fetch, RawFile};
use gw_nav::{AssetManifest, FileClient, Instance, MapDb, MapZone, PathingStore};
use progress::{Board, Line};

#[derive(Parser)]
#[command(about = "Guild Wars Navigator command line tool")]
struct Cli {
    /// Path to the MapDb sqlite file.
    #[arg(long, global = true, default_value = "mapfiles.db")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List all map zones.
    List,
    /// Search by name substring, or by mapid/mapfile if numeric.
    Search { query: String },
    /// Show a map zone's rows (outpost and explorable).
    Get { mapid: u32 },
    /// Insert or replace a map zone's outpost or explorable row.
    Set {
        mapid: u32,
        /// outpost or explorable.
        #[arg(long)]
        instance: Instance,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        mapfile: Option<u32>,
        #[arg(long)]
        unknown: Option<i64>,
    },
    /// Merge map zones from another sqlite db. Incoming rows overwrite
    /// existing ones only if they have a non-zero mapfile.
    Import { other: PathBuf },
    /// Download files from the fileserver and save them decompressed as
    /// <mapfile_id>.mapblob. Ids are resolved to their current revision
    /// through the asset manifest unless --exact is given.
    Download {
        #[arg(required = true)]
        mapfile_ids: Vec<u32>,
        /// Directory to write files into.
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
        /// Also save the compressed data as <mapfile_id>.cmp.
        #[arg(long)]
        raw: bool,
        /// Download the given ids as-is instead of their current revision.
        #[arg(long)]
        exact: bool,
        /// Directory for the cached asset manifest.
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
    },
    /// Print the file ids the fileserver announces on connect.
    Manifest,
    /// List the chunks of a downloaded FFNA file.
    Chunks { path: PathBuf },
    /// Dump the zones chunk (procedural foliage zones) of a stage-1 map
    /// file in readable form.
    ZoneChunk {
        path: PathBuf,
        /// Leave out the zone polygons' vertices.
        #[arg(long)]
        no_vertices: bool,
    },
    /// Download the prop models referenced by a map file (current
    /// revisions), saved as <base_id>.ffna. Existing files are kept.
    FetchModels {
        mapfile_id: u32,
        #[arg(long, default_value = "testdata/models")]
        out_dir: PathBuf,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
    },
    /// Load the pathing data of a map file: from the cache, or else
    /// download the map and its models and generate it.
    Pathing {
        mapfile_id: u32,
        /// Regenerate from the current revision even if cached.
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
        /// Also write the stage-2 path chunk to this file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Load the baked top-down render of a map file: from the cache, or
    /// else download its files and render it.
    Render {
        mapfile_id: u32,
        /// Re-render from the current revision even if cached.
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
        /// Also write the image to this file (JPEG).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Render the current revision at this many world units per pixel
        /// instead, without caching (needs --out; PNG or JPEG by extension).
        #[arg(long, requires = "out")]
        scale: Option<f32>,
    },
    /// Record the map files of the current asset manifest that no map zone
    /// names (table manifest_mapfiles). The pathing and render commands do
    /// this too whenever they load the manifest.
    ScanManifest {
        /// Also download the unchecked ones to check they are map files.
        #[arg(long)]
        verify: bool,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
    },
    /// Record the zone def .ini paths of every known map file (table
    /// mapfile_zone_defs), for the Maps list. Loading a map in the
    /// visualizer records its paths too.
    ScanZones {
        /// Only read map files already in the cache; don't download any.
        #[arg(long)]
        cached_only: bool,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
    },
    /// Download, bloat and render every known map file whose current
    /// revision isn't cached yet, like the game client's -image. Map files
    /// come from MapDb and the asset manifest's map file candidates.
    ImageAll {
        /// Map files processed at once.
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        /// Fileserver connections shared by all jobs. A job downloads over
        /// one connection at a time, with its requests pipelined.
        #[arg(long, default_value_t = 2)]
        connections: usize,
        /// Process at most this many map files.
        #[arg(long)]
        limit: Option<usize>,
        /// Only list the map files that would be processed.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "cache")]
        cache_dir: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Download {
        mapfile_ids,
        out_dir,
        raw,
        exact,
        cache_dir,
    } = &cli.command
    {
        return download(mapfile_ids, out_dir, *raw, (!*exact).then_some(cache_dir.as_path()));
    }
    if let Command::FetchModels { mapfile_id, out_dir, cache_dir } = &cli.command {
        return fetch_models(*mapfile_id, out_dir, cache_dir, &cli.db);
    }
    if let Command::Pathing { mapfile_id, refresh, cache_dir, out } = &cli.command {
        return pathing(*mapfile_id, *refresh, cache_dir, out.as_deref(), &cli.db);
    }
    if let Command::Render { mapfile_id, refresh, cache_dir, out, scale } = &cli.command {
        return match (scale, out) {
            (Some(scale), Some(out)) => render_preview(*mapfile_id, *scale, cache_dir, out, &cli.db),
            _ => render(*mapfile_id, *refresh, cache_dir, out.as_deref(), &cli.db),
        };
    }
    if let Command::ScanManifest { verify, cache_dir } = &cli.command {
        return scan_manifest(&cli.db, cache_dir, *verify);
    }
    if let Command::ScanZones { cached_only, cache_dir } = &cli.command {
        return scan_zones(&cli.db, cache_dir, *cached_only);
    }
    if let Command::ImageAll { jobs, connections, limit, dry_run, cache_dir } = &cli.command {
        return image_all(&cli.db, cache_dir, *jobs, *connections, *limit, *dry_run);
    }
    if let Command::Chunks { path } = &cli.command {
        return print_chunks(path);
    }
    if let Command::ZoneChunk { path, no_vertices } = &cli.command {
        return print_zone_chunk(path, !*no_vertices);
    }
    if let Command::Manifest = &cli.command {
        let client = FileClient::connect()?;
        for (i, id) in client.manifest().iter().enumerate() {
            println!("{i}: {id}");
        }
        return Ok(());
    }
    let mut db = MapDb::open(&cli.db)?;

    match cli.command {
        Command::List => print_zones(&db.all()?),
        Command::Search { query } => print_zones(&db.search(&query)?),
        Command::Get { mapid } => match db.get(mapid)? {
            zones if zones.is_empty() => println!("mapid {mapid} not found"),
            zones => print_zones(&zones),
        },
        Command::Set {
            mapid,
            instance,
            name,
            mapfile,
            unknown,
        } => {
            let zone = MapZone {
                mapid,
                instance,
                name,
                mapfile,
                unknown,
            };
            db.upsert(&zone)?;
            print_zones(&[zone]);
        }
        Command::Import { other } => {
            let report = db.import_from(&other)?;
            println!(
                "inserted {}, updated {}, unchanged {}, skipped {} (no mapfile), unplaced {} (no instance)",
                report.inserted.len(),
                report.updated.len(),
                report.unchanged,
                report.skipped.len(),
                report.unplaced.len()
            );
            for zone in &report.inserted {
                println!("  + {}", format_zone(zone));
            }
            for (old, new) in &report.updated {
                println!("  - {}", format_zone(old));
                println!("  + {}", format_zone(new));
            }
            for zone in &report.skipped {
                println!("  ! {}", format_zone(zone));
            }
            for zone in &report.unplaced {
                let mapfile = zone.mapfile.map_or("-".into(), |f| f.to_string());
                println!(
                    "  ? {:>6}  {:>10}  {:>8}  {}  (set its instance with `set --instance`)",
                    zone.mapid,
                    "",
                    mapfile,
                    zone.name.as_deref().unwrap_or("-")
                );
            }
        }
        Command::Download { .. }
        | Command::Manifest
        | Command::Chunks { .. }
        | Command::ZoneChunk { .. }
        | Command::FetchModels { .. }
        | Command::Pathing { .. }
        | Command::Render { .. }
        | Command::ScanManifest { .. }
        | Command::ScanZones { .. }
        | Command::ImageAll { .. } => {
            unreachable!("handled above")
        }
    }
    Ok(())
}

/// Load the asset manifest from `cache_dir`, downloading it if the cached
/// copy is missing or stale.
fn asset_manifest(client: &mut FileClient, cache_dir: &Path) -> Result<AssetManifest> {
    let id = client.asset_manifest_id();
    let path = cache_dir.join(format!("manifest-{id}.bin"));
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(_) => {
            let board = Board::new();
            let mut line = board.line();
            line.start(format!("asset manifest {id}"));
            let data = client.download(id, |done, total| line.progress(Progress::Bytes { file_id: id, done, total }))?;
            std::fs::create_dir_all(cache_dir)?;
            std::fs::write(&path, &data).with_context(|| format!("writing {}", path.display()))?;
            data
        }
    };
    Ok(AssetManifest::parse(&data)?)
}

fn download(file_ids: &[u32], out_dir: &Path, save_raw: bool, cache_dir: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut client = FileClient::connect()?;
    let manifest = cache_dir.map(|dir| asset_manifest(&mut client, dir)).transpose()?;
    let board = Board::with_total(file_ids.len());
    // The ids asked for, by the revision downloaded for them.
    let mut requested: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut revisions = Vec::new();
    for &id in file_ids {
        let current = manifest.as_ref().map_or(id, |m| m.resolve(id));
        if current != id {
            board.println(format!("{id}: current revision is {current}"));
        }
        requested.entry(current).or_default().push(id);
        revisions.push(current);
    }
    let mut line = board.line();
    let mut shown = None;
    let mut error = None;
    client.download_many(&revisions, |event| match event {
        Fetch::Bytes { file_id, done, total } => {
            if shown != Some(file_id) {
                shown = Some(file_id);
                let id = requested.get(&file_id).map_or(file_id, |ids| ids[0]);
                line.start(if file_id == id { id.to_string() } else { format!("{id} (r{file_id})") });
            }
            line.progress(Progress::Bytes { file_id, done, total });
        }
        Fetch::Done { file_id, result } => {
            for id in requested.remove(&file_id).unwrap_or_default() {
                board.inc(1);
                if error.is_none() {
                    error = save_download(id, &result, out_dir, save_raw, &board).err();
                }
            }
        }
    })?;
    error.map_or(Ok(()), Err)
}

/// Save a file [`download`] fetched for `id`, or report it missing.
fn save_download(
    id: u32,
    raw: &Result<RawFile, gw_nav::FileConnError>,
    out_dir: &Path,
    save_raw: bool,
    board: &Board,
) -> Result<()> {
    let raw = match raw {
        Ok(raw) => raw,
        Err(gw_nav::FileConnError::NotFound(_)) => {
            board.eprintln(format!("{id}: not found on fileserver"));
            return Ok(());
        }
        Err(e) => anyhow::bail!("downloading file {id}: {e}"),
    };
    board.println(format!(
        "{id}: compressed {} bytes, decompressed {} bytes, crc {:#010x}",
        raw.size_compressed, raw.size_decompressed, raw.crc
    ));
    if save_raw {
        let path = out_dir.join(format!("{id}.cmp"));
        std::fs::write(&path, &raw.data).with_context(|| format!("writing {}", path.display()))?;
    }
    let blob = raw
        .decompress()
        .with_context(|| format!("decompressing file {id}"))?;
    let path = out_dir.join(format!("{id}.mapblob"));
    std::fs::write(&path, &blob).with_context(|| format!("writing {}", path.display()))?;
    let magic = blob.get(..4).map(|m| String::from_utf8_lossy(m).into_owned());
    board.println(format!(
        "{id}: saved {} (magic {:?}, type {:?})",
        path.display(),
        magic.unwrap_or_default(),
        blob.get(4)
    ));
    Ok(())
}

fn fetch_models(mapfile_id: u32, out_dir: &Path, cache_dir: &Path, db: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut client = FileClient::connect()?;
    let manifest = asset_manifest(&mut client, cache_dir)?;
    record_manifest(db, client.asset_manifest_id(), &manifest);
    let map = {
        let board = Board::new();
        let mut line = board.line();
        line.start(format!("map file {mapfile_id}"));
        let file_id = manifest.resolve(mapfile_id);
        client.download(file_id, |done, total| line.progress(Progress::Bytes { file_id, done, total }))?
    };
    let map = Ffna::parse(&map)?;
    let refs = map.chunk(0x1100_0004).context("map has no prop file references")?;
    let ids = gw_nav::mapfile::parse_file_refs(refs)?;
    let board = Board::with_total(ids.len());
    // Models not saved yet, by the revision downloaded for them.
    let mut wanted: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut revisions = Vec::new();
    for &id in &ids {
        let file_id = manifest.resolve(id);
        let bases = wanted.entry(file_id).or_default();
        if bases.contains(&id) || out_dir.join(format!("{id}.ffna")).exists() {
            board.inc(1);
            continue;
        }
        bases.push(id);
        revisions.push(file_id);
    }
    let mut line = board.line();
    let (mut shown, mut fetched, mut error) = (None, 0, None);
    client.download_many(&revisions, |event| match event {
        Fetch::Bytes { file_id, done, total } => {
            if shown != Some(file_id) {
                shown = Some(file_id);
                let id = wanted.get(&file_id).map_or(file_id, |ids| ids[0]);
                line.start(format!("model {id}"));
            }
            line.progress(Progress::Bytes { file_id, done, total });
        }
        Fetch::Done { file_id, result } => {
            let data = result.and_then(|raw| raw.decompress().map_err(Into::into));
            for id in wanted.remove(&file_id).unwrap_or_default() {
                board.inc(1);
                match &data {
                    Ok(data) => match std::fs::write(out_dir.join(format!("{id}.ffna")), data) {
                        Ok(()) => fetched += 1,
                        Err(e) => {
                            error.get_or_insert(anyhow::Error::from(e).context(format!("writing model {id}")));
                        }
                    },
                    Err(gw_nav::FileConnError::NotFound(_)) => board.eprintln(format!("{id}: not found")),
                    Err(e) => {
                        error.get_or_insert(anyhow::anyhow!("model {id}: {e}"));
                    }
                }
            }
        }
    })?;
    drop((line, board));
    error.map_or(Ok(()), Err)?;
    println!("{} model refs, {fetched} downloaded", ids.len());
    Ok(())
}

/// Show a map file's [`Progress`] on a line of `board`, printing missing
/// files and render failures above it.
fn map_progress(board: &Board, name: String) -> impl FnMut(Progress) + '_ {
    let mut line = board.line();
    line.start(name);
    move |p| {
        match p {
            Progress::MissingFile(id) => board.eprintln(format!("file {id} not found, skipped")),
            Progress::RenderFailed => board.eprintln("rendering failed"),
            _ => {}
        }
        line.progress(p);
    }
}

/// Load the asset manifest of `store`, showing its progress.
fn load_manifest(store: &mut PathingStore) -> Result<()> {
    let board = Board::new();
    store.manifest(&mut map_progress(&board, "asset manifest".to_owned()))?;
    Ok(())
}

fn pathing(mapfile_id: u32, refresh: bool, cache_dir: &Path, out: Option<&Path>, db: &Path) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    let started = std::time::Instant::now();
    let board = Board::new();
    let progress = map_progress(&board, format!("map file {mapfile_id}"));
    let data = if refresh { store.fetch(mapfile_id, progress) } else { store.load(mapfile_id, progress) };
    drop(board);
    record_store_manifest(db, &store);
    let data = data?;
    println!(
        "{mapfile_id}: revision {}, {} planes, {} trapezoids, {} start points, {} obstacles ({:.2?})",
        data.file_id,
        data.planes.len(),
        data.trapezoid_count(),
        data.start_points.len(),
        data.obstacles.len(),
        started.elapsed()
    );
    if let Some(out) = out {
        let (_, cached) = store.cached_path(mapfile_id).context("no cache entry")?;
        std::fs::copy(&cached, out).with_context(|| format!("writing {}", out.display()))?;
    }
    Ok(())
}

fn print_chunks(path: &Path) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file = Ffna::parse(&data)?;
    println!("ffna type {}, {} chunks", file.file_type, file.chunks.len());
    for chunk in &file.chunks {
        let name = match ChunkId::parse(chunk.id) {
            Some(id) => format!("{:?}{}", id.kind, if id.file_refs { " file refs" } else { "" }),
            None => "?".into(),
        };
        let head: String = chunk.data.iter().take(12).map(|b| format!("{b:02x}")).collect();
        println!("{:#010x}  {:>9}  {:<24}  {head}", chunk.id, chunk.data.len(), name);
    }
    Ok(())
}

/// The map files of `map_zones` and `manifest_mapfiles`, less those checked
/// not to be maps, by id.
fn known_mapfiles(db: &MapDb) -> Result<Vec<u32>> {
    let mut mapfiles: Vec<u32> = db.all()?.iter().filter_map(|z| z.mapfile).filter(|&f| f != 0).collect();
    mapfiles.extend(db.manifest_mapfiles()?.iter().filter(|f| f.is_map != Some(false)).map(|f| f.mapfile));
    mapfiles.sort_unstable();
    mapfiles.dedup();
    Ok(mapfiles)
}

/// A map file for `image-all` to process.
struct ImageJob {
    mapfile: u32,
    /// The current revision.
    revision: u32,
    /// The pathing data isn't cached (generating it renders too).
    pathing: bool,
}

/// What a job did.
struct ImageDone {
    job: ImageJob,
    result: Result<()>,
    /// Models and textures missing from the fileserver.
    missing: usize,
    render_failed: bool,
    elapsed: Duration,
}

/// Bloat (generate the pathing data of) and render one map file, showing
/// its progress on `line`.
fn image_one(store: &mut PathingStore, job: ImageJob, line: &mut Line) -> ImageDone {
    let started = Instant::now();
    let (mut missing, mut render_failed) = (0, false);
    let mut progress = |p| {
        match p {
            Progress::MissingFile(_) => missing += 1,
            Progress::RenderFailed => render_failed = true,
            _ => {}
        }
        line.progress(p);
    };
    let result = if job.pathing {
        // Renders as well; a render failure is reported, not returned.
        store.load_chunk(job.mapfile, true, &mut progress).map(drop)
    } else {
        store.bake_render(job.mapfile, job.revision, &mut progress).map(drop)
    };
    ImageDone { job, result: result.map_err(Into::into), missing, render_failed, elapsed: started.elapsed() }
}

fn image_all(
    db_path: &Path,
    cache_dir: &Path,
    jobs: usize,
    connections: usize,
    limit: Option<usize>,
    dry_run: bool,
) -> Result<()> {
    let pool = Arc::new(ConnectionPool::fileserver(connections));
    let mut store = PathingStore::with_pool(cache_dir, pool);
    load_manifest(&mut store)?;
    record_store_manifest(db_path, &store);
    let (manifest_id, manifest) = store.loaded_manifest().context("no asset manifest loaded")?;
    let mut db = MapDb::open(db_path)?;
    let mut names: HashMap<u32, String> = HashMap::new();
    for zone in db.all()? {
        if let (Some(mapfile), Some(name)) = (zone.mapfile, zone.name) {
            names.entry(mapfile).or_insert(name);
        }
    }

    // The fileserver only has the map files the manifest lists.
    let (known, unlisted): (Vec<u32>, Vec<u32>) =
        known_mapfiles(&db)?.into_iter().partition(|&f| manifest.get(f).is_some());
    let mut queue: Vec<ImageJob> = known
        .iter()
        .map(|&mapfile| {
            let revision = manifest.resolve(mapfile);
            ImageJob { mapfile, revision, pathing: !store.has_pathing(mapfile, revision) }
        })
        .filter(|j| j.pathing || !store.has_render(j.mapfile, j.revision))
        .collect();
    let pathing = queue.iter().filter(|j| j.pathing).count();
    println!(
        "asset manifest {manifest_id}: {} map files, {} imaged, {} to do ({pathing} need pathing and a render, {} a render)",
        known.len(),
        known.len() - queue.len(),
        queue.len(),
        queue.len() - pathing,
    );
    if !unlisted.is_empty() {
        let ids: Vec<String> = unlisted.iter().map(|f| f.to_string()).collect();
        println!("skipped {} map files the asset manifest doesn't list: {}", unlisted.len(), ids.join(", "));
    }
    if let Some(limit) = limit {
        queue.truncate(limit);
    }
    let label = |mapfile: u32, revision: u32| match names.get(&mapfile) {
        Some(name) => format!("{mapfile} {name} (r{revision})"),
        None => format!("{mapfile} (r{revision})"),
    };
    if dry_run {
        for job in &queue {
            let what = if job.pathing { "pathing + render" } else { "render" };
            println!("  {}: {what}", label(job.mapfile, job.revision));
        }
        return Ok(());
    }
    if queue.is_empty() {
        return Ok(());
    }

    let total = queue.len();
    let workers = jobs.clamp(1, total);
    println!("imaging {total} map files with {workers} jobs over up to {connections} connections");
    // Pop from the end; keep the id order.
    queue.reverse();
    let queue = Mutex::new(queue);
    let (tx, rx) = std::sync::mpsc::channel::<ImageDone>();
    let started = Instant::now();
    let (mut done, mut failed) = (0, 0);
    let board = Board::with_total(total);
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let (tx, queue, mut store, mut line) = (tx.clone(), &queue, store.share(), board.line());
            scope.spawn(move || {
                loop {
                    let Some(job) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else { break };
                    line.start(label(job.mapfile, job.revision));
                    if tx.send(image_one(&mut store, job, &mut line)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Results arrive as jobs finish; MapDb is only touched here.
        for r in rx {
            done += 1;
            let ImageJob { mapfile, revision, pathing } = r.job;
            let file = store.cached_file(revision).and_then(|data| {
                let file = Ffna::parse(&data).ok()?;
                Some((file.file_type == TYPE_MAP, gw_nav::zones::zone_chunk(&file).ok()))
            });
            let mut notes = Vec::new();
            if r.missing > 0 {
                notes.push(format!("{} files missing", r.missing));
            }
            if r.render_failed {
                notes.push("render failed".to_owned());
            }
            let status = match &r.result {
                Ok(()) => {
                    let what = if pathing { "pathing + render" } else { "render" };
                    format!("{what}{}", notes.iter().map(|n| format!(", {n}")).collect::<String>())
                }
                Err(e) => {
                    failed += 1;
                    format!("FAILED: {e:#}")
                }
            };
            board.println(format!("[{done:>4}/{total}] {}: {status} ({:.1?})", label(mapfile, revision), r.elapsed));
            board.inc(1);
            match file {
                // Not a map after all; leave it out next time.
                Some((false, _)) => db.set_is_map(mapfile, false)?,
                Some((true, Some(chunk))) => {
                    let defs: Vec<(u32, &str)> = chunk.defs.iter().map(|d| (d.id, d.ini_path.as_str())).collect();
                    db.record_zone_defs(mapfile, revision, &defs)?;
                }
                _ => {}
            }
        }
        Ok(())
    })?;
    drop(board);
    println!("imaged {} of {total} map files in {:.1?}", done - failed, started.elapsed());
    anyhow::ensure!(failed == 0, "{failed} map files failed");
    Ok(())
}

fn scan_zones(db_path: &Path, cache_dir: &Path, cached_only: bool) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    load_manifest(&mut store)?;
    record_store_manifest(db_path, &store);
    let (_, manifest) = store.loaded_manifest().context("no asset manifest loaded")?;
    let mut db = MapDb::open(db_path)?;
    let files: Vec<(u32, u32)> = known_mapfiles(&db)?.into_iter().map(|f| (f, manifest.resolve(f))).collect();

    let (mut changed, mut unchanged, mut skipped, mut failed) = (0, 0, 0, 0);
    let board = Board::with_total(files.len());
    let mut line = board.line();
    for &(mapfile, revision) in &files {
        board.inc(1);
        line.start(format!("{mapfile} (r{revision})"));
        let data = if cached_only {
            match store.cached_file(revision) {
                Some(data) => data,
                None => {
                    skipped += 1;
                    continue;
                }
            }
        } else {
            match store.map_file_with_progress(revision, |p| line.progress(p)) {
                Ok(data) => data,
                Err(e) => {
                    failed += 1;
                    board.eprintln(format!("{mapfile} (revision {revision}): {e}"));
                    continue;
                }
            }
        };
        let chunk = Ffna::parse(&data).map_err(anyhow::Error::from).and_then(|file| {
            anyhow::ensure!(file.file_type == TYPE_MAP, "not a map file");
            Ok(gw_nav::zones::zone_chunk(&file)?)
        });
        match chunk {
            Ok(chunk) => {
                let defs: Vec<(u32, &str)> = chunk.defs.iter().map(|d| (d.id, d.ini_path.as_str())).collect();
                if db.record_zone_defs(mapfile, revision, &defs)? {
                    changed += 1;
                } else {
                    unchanged += 1;
                }
            }
            Err(e) => {
                failed += 1;
                board.eprintln(format!("{mapfile} (revision {revision}): {e}"));
            }
        }
    }
    drop((line, board));
    println!(
        "{} map files: {changed} recorded or updated, {unchanged} unchanged, {skipped} not cached, {failed} failed",
        files.len()
    );
    Ok(())
}

fn print_zone_chunk(path: &Path, vertices: bool) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file = Ffna::parse(&data)?;
    let chunk = file.chunk(0x1000_0003).context("no stage-1 zones chunk (0x10000003)")?;
    let refs = match file.chunk(0x1100_0003) {
        Some(refs) => parse_file_refs(refs)?,
        None => Vec::new(),
    };
    let zones = ZonesStrip::parse(chunk)?;
    let file_id = |i: usize| refs.get(i).map_or("?".into(), |id| id.to_string());

    println!("zones chunk: {} bytes, version {}, {} file refs", chunk.len(), zones.version, refs.len());
    for s in &zones.sections {
        let name = match s.tag {
            zones::tag::DEFS => "zone defs",
            zones::tag::PROP_FILES => "prop files",
            zones::tag::ZONES => "zones",
            0xFF => "end",
            _ => "unknown",
        };
        println!("  @{:#06x}  tag {:#04x}  {:>6} bytes  {name}", s.offset, s.tag, s.len);
    }

    let starts = zones.model_starts();
    if starts.last() != Some(&refs.len()) {
        println!("warning: the defs have {} models but the file-ref list has {}", starts.last().unwrap(), refs.len());
    }
    println!("\nzone defs: {}", zones.defs.len());
    for (def, &start) in zones.defs.iter().zip(&starts) {
        let used = zones.zones.iter().filter(|z| z.def_id == def.id).count();
        println!(
            "def {}  {}  ({} layers, {} models from ref {start}, used by {used} zones)",
            def.id,
            def.ini_path,
            def.layers.len(),
            def.models.len(),
        );
        let mut index = start;
        for (i, (layer, models)) in def.layer_models().enumerate() {
            println!(
                "  layer {i}: kind {}  level {}  spacing {}  collision {}  density {}  scale variance {}  pattern {}  {} models",
                layer.kind,
                layer.level(),
                layer.spacing,
                layer.collision_radius,
                layer.density,
                layer.scale_variance,
                layer.pattern,
                layer.model_count,
            );
            let mut previous = 0.0;
            for model in models {
                println!(
                    "    ref {index:>3}  file {:>7}  p {:.3}  (cumulative {:.3})  flags {:#06x}",
                    file_id(index),
                    model.cumulative_probability - previous,
                    model.cumulative_probability,
                    model.flags,
                );
                previous = model.cumulative_probability;
                index += 1;
            }
        }
    }

    if let Some(props) = &zones.prop_files {
        // One atlas nibble per model, low nibble first.
        let nibbles: Vec<u8> = props.atlas.iter().flat_map(|b| [b & 0xF, b >> 4]).collect();
        println!("\nprop files: {} models", props.models.len());
        for (k, &i) in props.models.iter().enumerate() {
            let atlas = nibbles.get(k).map_or("?".into(), |n| format!("{n:#x}"));
            println!("  ref {i:>3}  file {:>7}  atlas {atlas}", file_id(i as usize));
        }
        let hex: Vec<_> = props.atlas.iter().map(|b| format!("{b:02x}")).collect();
        println!("  atlas bytes: {}", hex.join(" "));
    }

    println!("\nzones: {}", zones.zones.len());
    for (i, zone) in zones.zones.iter().enumerate() {
        let area = zone.signed_area();
        let bounds = zone
            .bounds()
            .map_or("-".into(), |(lo, hi)| format!("x {:.0}..{:.0}, y {:.0}..{:.0}", lo[0], hi[0], lo[1], hi[1]));
        println!(
            "zone {i:>3}  def {}  flags {:#04x}  height {} ({:#06x})  {} vertices  area {:.0} {}  {bounds}",
            zone.def_id,
            zone.flags,
            zone.height(),
            zone.height_raw,
            zone.vertices.len(),
            area.abs(),
            if area < 0.0 { "cw" } else { "ccw" },
        );
        if vertices {
            let points: Vec<_> = zone.vertices.iter().map(|p| format!("({:.1}, {:.1})", p[0], p[1])).collect();
            for line in points.chunks(6) {
                println!("      {}", line.join(" "));
            }
        }
    }

    for (tag, payload) in &zones.unknown {
        let hex: String = payload.iter().take(64).map(|b| format!("{b:02x}")).collect();
        println!("\nunknown tag {tag:#04x}, {} bytes: {hex}", payload.len());
    }
    Ok(())
}

fn print_zones(zones: &[MapZone]) {
    println!("{:>6}  {:>10}  {:>8}  {:>7}  name", "mapid", "instance", "mapfile", "unknown");
    for zone in zones {
        println!("{}", format_zone(zone));
    }
}

fn format_zone(zone: &MapZone) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    format!(
        "{:>6}  {:>10}  {:>8}  {:>7}  {}",
        zone.mapid,
        zone.instance.as_str(),
        opt(zone.mapfile.map(|v| v.to_string())),
        opt(zone.unknown.map(|v| v.to_string())),
        zone.name.as_deref().unwrap_or("-"),
    )
}

fn render(mapfile_id: u32, refresh: bool, cache_dir: &Path, out: Option<&Path>, db: &Path) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    let started = std::time::Instant::now();
    let board = Board::new();
    let loaded = store.load_render(mapfile_id, refresh, map_progress(&board, format!("map file {mapfile_id}")));
    drop(board);
    record_store_manifest(db, &store);
    let (file_id, data) = loaded?;
    let render = gw_nav::render::WorldRender::decode(&data)?;
    println!(
        "{mapfile_id}: revision {file_id}, {} KiB, {}x{}, {} props, {} sprites, bounds {:?} ({:.2?})",
        data.len() / 1024,
        render.width,
        render.height,
        render.props.len(),
        render.sprites.len(),
        render.bounds,
        started.elapsed()
    );
    if let Some(out) = out {
        save_render(&render, out)?;
    }
    Ok(())
}

fn render_preview(mapfile_id: u32, scale: f32, cache_dir: &Path, out: &Path, db: &Path) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    let started = std::time::Instant::now();
    let board = Board::new();
    let render = store.render_preview(mapfile_id, scale, map_progress(&board, format!("map file {mapfile_id}")));
    drop(board);
    record_store_manifest(db, &store);
    let render = render?;
    save_render(&render, out)?;
    println!("{mapfile_id}: {}x{} ({:.2?})", render.width, render.height, started.elapsed());
    Ok(())
}

/// Save a render with every prop, in the format `out`'s extension names.
fn save_render(render: &gw_nav::render::WorldRender, out: &Path) -> Result<()> {
    let (w, h) = (render.width as u32, render.height as u32);
    image::save_buffer(out, &render.compose_all(), w, h, image::ColorType::Rgb8)
        .with_context(|| format!("writing {}", out.display()))
}

/// Record the map files of asset manifest `id` in MapDb, as `scan-manifest`
/// does. A failure here doesn't fail the command.
fn record_manifest(db: &Path, id: u32, manifest: &AssetManifest) {
    match MapDb::open(db).and_then(|mut d| d.record_manifest(id, &manifest.map_file_candidates())) {
        Ok(Some(new)) if new > 0 => eprintln!("asset manifest {id}: {new} new map files without a map zone recorded in {}", db.display()),
        Ok(_) => {}
        Err(e) => eprintln!("recording asset manifest map files in {}: {e}", db.display()),
    }
}

/// [`record_manifest`] for the manifest `store` loaded, if it loaded one.
fn record_store_manifest(db: &Path, store: &PathingStore) {
    if let Some((id, manifest)) = store.loaded_manifest() {
        record_manifest(db, id, manifest);
    }
}

fn scan_manifest(db_path: &Path, cache_dir: &Path, verify: bool) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    load_manifest(&mut store)?;
    let (id, manifest) = store.loaded_manifest().context("no asset manifest loaded")?;
    let candidates = manifest.map_file_candidates();
    let mut db = MapDb::open(db_path)?;
    let new = db.record_manifest(id, &candidates)?;
    let listed: HashSet<u32> = db.all()?.iter().filter_map(|z| z.mapfile).collect();
    let known = candidates.iter().filter(|c| listed.contains(&c.base_id)).count();
    println!(
        "asset manifest {id}: {} entries, {} map file candidates, {known} named in map_zones, {} not",
        manifest.len(),
        candidates.len(),
        candidates.len() - known
    );
    match new {
        Some(new) => println!("{new} new map files without a map zone recorded in {}", db_path.display()),
        None => println!("this manifest was recorded before"),
    }
    if !verify {
        return Ok(());
    }

    let unchecked: Vec<_> = db.unlisted_mapfiles()?.into_iter().filter(|f| f.is_map.is_none()).collect();
    let (mut maps, mut others, mut failed) = (0, 0, 0);
    let board = Board::with_total(unchecked.len());
    let mut line = board.line();
    for f in &unchecked {
        board.inc(1);
        line.start(format!("{} (r{})", f.mapfile, f.revision));
        match store.map_file_with_progress(f.revision, |p| line.progress(p)) {
            Ok(data) => {
                let is_map = Ffna::parse(&data).is_ok_and(|file| file.file_type == TYPE_MAP);
                db.set_is_map(f.mapfile, is_map)?;
                if is_map {
                    maps += 1;
                } else {
                    others += 1;
                    board.eprintln(format!("{} (revision {}): not a map file", f.mapfile, f.revision));
                }
            }
            Err(e) => {
                failed += 1;
                board.eprintln(format!("{} (revision {}): {e}", f.mapfile, f.revision));
            }
        }
    }
    drop((line, board));
    println!("checked {}: {maps} map files, {others} other files, {failed} failed", unchecked.len());
    Ok(())
}
