use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gw_nav::mapfile::ffna::TYPE_MAP;
use gw_nav::mapfile::{ChunkId, Ffna};
use gw_nav::pathing::Progress;
use gw_nav::{AssetManifest, FileClient, MapDb, MapZone, PathingStore};

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
    /// Show a single map zone.
    Get { mapid: u32 },
    /// Insert or replace a map zone.
    Set {
        mapid: u32,
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
    if let Command::Chunks { path } = &cli.command {
        return print_chunks(path);
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
            Some(zone) => print_zones(&[zone]),
            None => println!("mapid {mapid} not found"),
        },
        Command::Set {
            mapid,
            name,
            mapfile,
            unknown,
        } => {
            let zone = MapZone {
                mapid,
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
                "inserted {}, updated {}, unchanged {}, skipped {} (no mapfile)",
                report.inserted.len(),
                report.updated.len(),
                report.unchanged,
                report.skipped.len()
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
        }
        Command::Download { .. }
        | Command::Manifest
        | Command::Chunks { .. }
        | Command::FetchModels { .. }
        | Command::Pathing { .. }
        | Command::Render { .. }
        | Command::ScanManifest { .. } => {
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
            eprintln!("downloading asset manifest {id}");
            let data = client.download(id, |_, _| {})?;
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
    for &id in file_ids {
        let current = manifest.as_ref().map_or(id, |m| m.resolve(id));
        if current != id {
            println!("{id}: current revision is {current}");
        }
        let raw = match client.download_raw(current, |done, total| {
            eprint!("\r{id}: {:5.1}% ({done}/{total} bytes)", 100.0 * done as f64 / total.max(1) as f64);
            let _ = std::io::stderr().flush();
        }) {
            Ok(raw) => raw,
            Err(gw_nav::FileConnError::NotFound(_)) => {
                eprintln!("{id}: not found on fileserver");
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        eprintln!();
        println!(
            "{id}: compressed {} bytes, decompressed {} bytes, crc {:#010x}",
            raw.size_compressed, raw.size_decompressed, raw.crc
        );
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
        println!(
            "{id}: saved {} (magic {:?}, type {:?})",
            path.display(),
            magic.unwrap_or_default(),
            blob.get(4)
        );
    }
    Ok(())
}

fn fetch_models(mapfile_id: u32, out_dir: &Path, cache_dir: &Path, db: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut client = FileClient::connect()?;
    let manifest = asset_manifest(&mut client, cache_dir)?;
    record_manifest(db, client.asset_manifest_id(), &manifest);
    let map = client.download(manifest.resolve(mapfile_id), |_, _| {})?;
    let map = Ffna::parse(&map)?;
    let refs = map.chunk(0x1100_0004).context("map has no prop file references")?;
    let ids = gw_nav::mapfile::parse_file_refs(refs)?;
    let mut fetched = 0;
    for id in &ids {
        let path = out_dir.join(format!("{id}.ffna"));
        if path.exists() {
            continue;
        }
        match client.download(manifest.resolve(*id), |_, _| {}) {
            Ok(data) => {
                std::fs::write(&path, data)?;
                fetched += 1;
            }
            Err(gw_nav::FileConnError::NotFound(_)) => eprintln!("{id}: not found"),
            Err(e) => return Err(e.into()),
        }
    }
    println!("{} model refs, {fetched} downloaded", ids.len());
    Ok(())
}

fn print_progress(p: Progress) {
    match p {
        Progress::Connecting => eprintln!("connecting to the fileserver"),
        Progress::Manifest => eprintln!("downloading the asset manifest"),
        Progress::Map(done, total) => {
            eprint!("\rmap file: {:5.1}%", 100.0 * done as f64 / total.max(1) as f64);
            if done == total {
                eprintln!();
            }
        }
        Progress::Models(done, total) => {
            eprint!("\rmodels: {done}/{total}");
            if done == total {
                eprintln!();
            }
        }
        Progress::MissingFile(id) => eprintln!("\nfile {id} not found, skipped"),
        Progress::Generating => eprintln!("generating"),
        Progress::RenderFiles(done, total) => {
            eprint!("\rrender files: {done}/{total}");
            if done == total {
                eprintln!();
            }
        }
        Progress::Rendering => eprintln!("rendering"),
        Progress::RenderFailed => eprintln!("rendering failed"),
    }
    let _ = std::io::stderr().flush();
}

fn pathing(mapfile_id: u32, refresh: bool, cache_dir: &Path, out: Option<&Path>, db: &Path) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    let started = std::time::Instant::now();
    let data = if refresh { store.fetch(mapfile_id, print_progress) } else { store.load(mapfile_id, print_progress) };
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

fn print_zones(zones: &[MapZone]) {
    println!("{:>6}  {:>8}  {:>7}  name", "mapid", "mapfile", "unknown");
    for zone in zones {
        println!("{}", format_zone(zone));
    }
}

fn format_zone(zone: &MapZone) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    format!(
        "{:>6}  {:>8}  {:>7}  {}",
        zone.mapid,
        opt(zone.mapfile.map(|v| v.to_string())),
        opt(zone.unknown.map(|v| v.to_string())),
        zone.name.as_deref().unwrap_or("-"),
    )
}

fn render(mapfile_id: u32, refresh: bool, cache_dir: &Path, out: Option<&Path>, db: &Path) -> Result<()> {
    let mut store = PathingStore::new(cache_dir);
    let started = std::time::Instant::now();
    let loaded = store.load_render(mapfile_id, refresh, print_progress);
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
    let render = store.render_preview(mapfile_id, scale, print_progress);
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
    store.manifest(&mut print_progress)?;
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
    for (i, f) in unchecked.iter().enumerate() {
        eprint!("\rchecking {}/{}", i + 1, unchecked.len());
        let _ = std::io::stderr().flush();
        match store.map_file(f.revision) {
            Ok(data) => {
                let is_map = Ffna::parse(&data).is_ok_and(|file| file.file_type == TYPE_MAP);
                db.set_is_map(f.mapfile, is_map)?;
                if is_map {
                    maps += 1;
                } else {
                    others += 1;
                    eprintln!("\n{} (revision {}): not a map file", f.mapfile, f.revision);
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("\n{} (revision {}): {e}", f.mapfile, f.revision);
            }
        }
    }
    eprintln!();
    println!("checked {}: {maps} map files, {others} other files, {failed} failed", unchecked.len());
    Ok(())
}
