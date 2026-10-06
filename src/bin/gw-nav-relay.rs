//! Relay server for the web build of the visualizer (see `gw_nav::api`).
//!
//! Browsers can't reach the Guild Wars fileserver (raw TCP), so this serves
//! MapDb, pathing data and baked map renders over HTTP: they come from the
//! local cache, or are downloaded and generated on demand exactly as the
//! desktop app does.
//! It also serves the web app's static files from `--web-dir`.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use clap::Parser;
use gw_nav::api;
use gw_nav::PathingStore;
use tiny_http::{Header, Method, Request, Response, Server};

#[derive(Parser)]
#[command(about = "HTTP relay serving MapDb and on-demand pathing data to the web visualizer")]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    addr: String,
    /// Path to the MapDb sqlite file.
    #[arg(long, default_value = "mapfiles.db")]
    db: PathBuf,
    /// Cache directory for downloads and generated pathing data.
    #[arg(long, default_value = "cache")]
    cache_dir: PathBuf,
    /// Directory with the web app (index.html, the wasm bundle).
    #[arg(long, default_value = "web")]
    web_dir: PathBuf,
    /// gwbs zones.db with recorded zone exits (table `zone_exit`).
    #[arg(long)]
    zones_db: Option<PathBuf>,
}

struct State {
    db: PathBuf,
    web_dir: PathBuf,
    zones_db: Option<PathBuf>,
    /// Generation is serialized; other requests are served meanwhile.
    store: Mutex<PathingStore>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let server = Server::http(&cli.addr).map_err(|e| anyhow!("listening on {}: {e}", cli.addr))?;
    eprintln!("relay listening on http://{}", cli.addr);
    let state = Arc::new(State {
        db: cli.db,
        web_dir: cli.web_dir,
        zones_db: cli.zones_db,
        store: Mutex::new(PathingStore::new(cli.cache_dir)),
    });
    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let (method, url) = (request.method().clone(), request.url().to_owned());
            let started = std::time::Instant::now();
            match handle(&state, request) {
                Ok(status) => eprintln!("{method} {url} -> {status} ({:.2?})", started.elapsed()),
                Err(e) => eprintln!("{method} {url} -> failed to respond: {e}"),
            }
        });
    }
    Ok(())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

/// Allow a web app served elsewhere (e.g. a dev server) to use the API.
fn cors<R: Read>(response: Response<R>) -> Response<R> {
    response
        .with_header(header("Access-Control-Allow-Origin", "*"))
        .with_header(header("Access-Control-Expose-Headers", api::FILE_ID_HEADER))
}

fn text(status: u16, body: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(status).with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

/// Answer one request; returns the status code sent.
fn handle(state: &State, request: Request) -> std::io::Result<u16> {
    if *request.method() == Method::Options {
        let response = cors(Response::empty(204))
            .with_header(header("Access-Control-Allow-Methods", "GET, OPTIONS"))
            .with_header(header("Access-Control-Allow-Headers", "*"));
        request.respond(response)?;
        return Ok(204);
    }
    if !matches!(request.method(), Method::Get | Method::Head) {
        request.respond(cors(text(405, "only GET and HEAD are supported")))?;
        return Ok(405);
    }
    let url = request.url().to_owned();
    let response = if url == api::MAPS_PATH {
        maps(state)
    } else if let Some((mapfile_id, refresh)) = api::parse_pathing_request(&url) {
        pathing(state, mapfile_id, refresh)
    } else if let Some(mapfile_id) = api::parse_annotations_request(&url) {
        annotations(state, mapfile_id)
    } else if let Some((mapfile_id, refresh)) = api::parse_render_request(&url) {
        render(state, mapfile_id, refresh)
    } else if url.starts_with("/api/") {
        text(404, "unknown API path")
    } else {
        static_file(&state.web_dir, &url)
    };
    let status = response.status_code().0;
    request.respond(cors(response))?;
    Ok(status)
}

fn maps(state: &State) -> Response<std::io::Cursor<Vec<u8>>> {
    match gw_nav::zones::map_list(&state.db) {
        Ok(entries) => Response::from_data(serde_json::to_vec(&entries).expect("serializable"))
            .with_header(header("Content-Type", "application/json")),
        Err(e) => text(500, format!("MapDb {}: {e}", state.db.display())),
    }
}

fn pathing(state: &State, mapfile_id: u32, refresh: bool) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let loaded = store.load_chunk(mapfile_id, refresh, |_| {});
    record_manifest(state, &store);
    match loaded {
        Ok((file_id, chunk)) => Response::from_data(chunk)
            .with_header(header("Content-Type", "application/octet-stream"))
            .with_header(header(api::FILE_ID_HEADER, &file_id.to_string())),
        Err(e) => text(502, format!("map file {mapfile_id}: {e}")),
    }
}

fn render(state: &State, mapfile_id: u32, refresh: bool) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let loaded = store.load_render(mapfile_id, refresh, |_| {});
    record_manifest(state, &store);
    match loaded {
        Ok((file_id, data)) => Response::from_data(data)
            .with_header(header("Content-Type", "application/octet-stream"))
            .with_header(header(api::FILE_ID_HEADER, &file_id.to_string())),
        Err(e) => text(502, format!("map file {mapfile_id}: {e}")),
    }
}

/// Record the map files of a newly loaded asset manifest in MapDb, so the
/// map list shows the ones it has no row for.
fn record_manifest(state: &State, store: &PathingStore) {
    match gw_nav::zones::record_manifest_mapfiles(&state.db, store) {
        Ok(Some(new)) => eprintln!("asset manifest: {new} new map files without a map zone recorded in {}", state.db.display()),
        Ok(None) => {}
        Err(e) => eprintln!("recording asset manifest map files in {}: {e}", state.db.display()),
    }
}

/// The annotations of the cached revision of a map file (see
/// `gw_nav::zones`); 404 before its pathing data has been loaded.
fn annotations(state: &State, mapfile_id: u32) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let Some((file_id, _)) = store.cached_path(mapfile_id) else {
        return text(404, format!("map file {mapfile_id} is not loaded"));
    };
    let a = gw_nav::zones::annotations(&mut store, &state.db, state.zones_db.as_deref(), mapfile_id, file_id);
    Response::from_data(serde_json::to_vec(&a).expect("serializable")).with_header(header("Content-Type", "application/json"))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// A file under `root`, or 404. Paths that leave `root` are rejected.
fn static_file(root: &Path, url: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match resolve(root, url).and_then(|path| Some((std::fs::read(&path).ok()?, path))) {
        Some((data, path)) => Response::from_data(data).with_header(header("Content-Type", content_type(&path))),
        None => text(404, "not found"),
    }
}

fn resolve(root: &Path, url: &str) -> Option<PathBuf> {
    let path = url.split(['?', '#']).next().unwrap_or("/");
    let path = if path.ends_with('/') { format!("{path}index.html") } else { path.to_owned() };
    let relative = Path::new(path.trim_start_matches('/'));
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(relative))
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use gw_nav::MapDb;
    use gw_nav::api::MapEntry;

    use super::*;

    #[test]
    fn static_paths() {
        let root = Path::new("web");
        assert_eq!(resolve(root, "/"), Some(root.join("index.html")));
        assert_eq!(resolve(root, "/pkg/gw-nav.js?v=1"), Some(root.join("pkg/gw-nav.js")));
        assert_eq!(resolve(root, "/../Cargo.toml"), None);
        assert_eq!(resolve(root, "/pkg/../../x"), None);
        assert_eq!(content_type(Path::new("a/b.wasm")), "application/wasm");
    }

    #[test]
    fn serves_maps_and_errors() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("maps.db");
        MapDb::open(&db)?.upsert(&gw_nav::MapZone {
            mapid: 546,
            name: Some("Jaga Moraine".into()),
            mapfile: Some(290943),
            unknown: None,
        })?;
        let state = State {
            db,
            web_dir: dir.path().to_owned(),
            zones_db: None,
            store: Mutex::new(PathingStore::new(dir.path().join("cache"))),
        };
        let mut body = String::new();
        maps(&state).into_reader().read_to_string(&mut body).context("reading body")?;
        let entries: Vec<MapEntry> = serde_json::from_str(&body)?;
        assert_eq!(entries, vec![MapEntry { mapid: Some(546), name: Some("Jaga Moraine".into()), mapfile: Some(290943) }]);
        assert_eq!(static_file(dir.path(), "/missing.html").status_code().0, 404);
        assert_eq!(annotations(&state, 290943).status_code().0, 404);
        Ok(())
    }
}
