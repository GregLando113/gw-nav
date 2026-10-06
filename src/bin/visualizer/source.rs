//! Where the visualizer gets its data: locally (MapDb file and
//! `PathingStore` on a background thread) or from a relay server over HTTP.
//! The web build always uses a relay.

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;
use gw_nav::PathingData;
use gw_nav::api::{self, MapAnnotations, MapEntry};
use gw_nav::render::WorldRender;

pub enum Event {
    /// A human-readable status line, with a completion fraction if known.
    Progress(String, Option<f32>),
    Loaded(Box<PathingData>),
    Failed(u32, String),
    Maps(Result<Vec<MapEntry>, String>),
    /// Sent after [`Event::Loaded`] for the same map file.
    Annotations(u32, Box<MapAnnotations>),
    /// The map file's baked top-down render, sent after
    /// [`Event::Annotations`].
    Background(u32, Result<Box<WorldRender>, String>),
}

enum Backend {
    #[cfg(not(target_arch = "wasm32"))]
    Local { db: std::path::PathBuf, requests: Sender<(u32, bool)> },
    Relay { base: String },
}

pub struct Source {
    backend: Backend,
    events_tx: Sender<Event>,
    events: Receiver<Event>,
    ctx: egui::Context,
    /// The map file id being loaded, if any.
    pub busy: Option<u32>,
}

impl Source {
    /// Load locally: MapDb from `db`, pathing through a `PathingStore` in
    /// `cache_dir`, zone exits from `zones_db`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn local(
        db: std::path::PathBuf,
        cache_dir: std::path::PathBuf,
        zones_db: Option<std::path::PathBuf>,
        ctx: egui::Context,
    ) -> Self {
        let (events_tx, events) = channel();
        let (requests, request_rx) = channel();
        let (tx, worker_ctx, worker_db) = (events_tx.clone(), ctx.clone(), db.clone());
        std::thread::Builder::new()
            .name("pathing-loader".into())
            .spawn(move || local::run(cache_dir, worker_db, zones_db, request_rx, tx, worker_ctx))
            .expect("spawning the loader thread");
        Self { backend: Backend::Local { db, requests }, events_tx, events, ctx, busy: None }
    }

    /// Load from the relay server at `base` (e.g. `http://127.0.0.1:8080`).
    pub fn relay(base: String, ctx: egui::Context) -> Self {
        let (events_tx, events) = channel();
        Self { backend: Backend::Relay { base }, events_tx, events, ctx, busy: None }
    }

    pub fn describe(&self) -> String {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Local { db, .. } => format!("local ({})", db.display()),
            Backend::Relay { base } => format!("relay {base}"),
        }
    }

    fn send(&self, event: Event) {
        let _ = self.events_tx.send(event);
        self.ctx.request_repaint();
    }

    /// Whether map rows can be imported from another MapDb (local data
    /// only; the relay has no import).
    pub fn can_import(&self) -> bool {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Local { .. } => true,
            Backend::Relay { .. } => false,
        }
    }

    /// Ask for another MapDb file and merge its rows into ours, as the CLI's
    /// `import` does. Returns the file and a summary of the merge, or `None`
    /// if cancelled. The map list needs requesting again afterwards.
    pub fn import_maps(&self) -> Option<(String, Result<String, String>)> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Local { db, .. } => {
                let dir = std::path::absolute(db).ok().and_then(|p| Some(p.parent()?.to_path_buf()));
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Import map rows from another MapDb")
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                    .add_filter("All files", &["*"]);
                if let Some(dir) = dir {
                    dialog = dialog.set_directory(dir);
                }
                let other = dialog.pick_file()?;
                let report = gw_nav::MapDb::open(db).and_then(|mut db| db.import_from(&other));
                let summary = report
                    .map(|r| {
                        format!(
                            "inserted {}, updated {}, unchanged {}, skipped {} (no mapfile)",
                            r.inserted.len(),
                            r.updated.len(),
                            r.unchanged,
                            r.skipped.len()
                        )
                    })
                    .map_err(|e| e.to_string());
                Some((other.display().to_string(), summary))
            }
            Backend::Relay { .. } => None,
        }
    }

    /// Request the map list; answered with [`Event::Maps`].
    pub fn request_maps(&self) {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Local { db, .. } => {
                self.send(Event::Maps(local::map_list(db)));
            }
            Backend::Relay { base } => {
                let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
                ehttp::fetch(ehttp::Request::get(api::maps_url(base)), move |result| {
                    let maps = checked(result)
                        .and_then(|r| serde_json::from_slice(&r.bytes).map_err(|e| format!("bad map list: {e}")));
                    let _ = tx.send(Event::Maps(maps));
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Load a map's pathing data; answered with [`Event::Loaded`] or
    /// [`Event::Failed`], with [`Event::Progress`] in between.
    pub fn load(&mut self, mapfile_id: u32, refresh: bool) {
        self.busy = Some(mapfile_id);
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Local { requests, .. } => {
                if requests.send((mapfile_id, refresh)).is_err() {
                    self.busy = None;
                }
            }
            Backend::Relay { base } => {
                self.send(Event::Progress(
                    format!("Loading map file {mapfile_id} from the relay (generating can take a few seconds)"),
                    None,
                ));
                let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
                let request = ehttp::Request::get(api::pathing_url(base, mapfile_id, refresh));
                let annotations = api::annotations_url(base, mapfile_id);
                let render = api::render_url(base, mapfile_id, false);
                ehttp::fetch(request, move |result| {
                    let loaded = checked(result).and_then(|r| decode(mapfile_id, &r));
                    let ok = loaded.is_ok();
                    let _ = tx.send(match loaded {
                        Ok(data) => Event::Loaded(Box::new(data)),
                        Err(e) => Event::Failed(mapfile_id, e),
                    });
                    ctx.request_repaint();
                    if ok {
                        ehttp::fetch(ehttp::Request::get(annotations), move |result| {
                            let a = checked(result)
                                .and_then(|r| serde_json::from_slice(&r.bytes).map_err(|e| format!("bad annotations: {e}")))
                                .unwrap_or_else(|e| MapAnnotations { notes: vec![e], ..Default::default() });
                            let _ = tx.send(Event::Annotations(mapfile_id, Box::new(a)));
                            ctx.request_repaint();
                            ehttp::fetch(ehttp::Request::get(render), move |result| {
                                let image = checked(result).and_then(|r| decode_render(&r.bytes));
                                let _ = tx.send(Event::Background(mapfile_id, image));
                                ctx.request_repaint();
                            });
                        });
                    }
                });
            }
        }
    }

    /// Events received since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<Event> = self.events.try_iter().collect();
        if events.iter().any(|e| matches!(e, Event::Loaded(_) | Event::Failed(..))) {
            self.busy = None;
        }
        events
    }
}

/// A successful response, or the error text (the relay puts its error
/// message in the body).
fn checked(result: ehttp::Result<ehttp::Response>) -> Result<ehttp::Response, String> {
    let response = result?;
    if response.ok {
        Ok(response)
    } else {
        let body = String::from_utf8_lossy(&response.bytes);
        Err(format!("HTTP {}: {}", response.status, body.trim()))
    }
}

fn decode_render(data: &[u8]) -> Result<Box<WorldRender>, String> {
    WorldRender::decode(data).map(Box::new).map_err(|e| format!("bad map render: {e}"))
}

fn decode(mapfile_id: u32, response: &ehttp::Response) -> Result<PathingData, String> {
    let file_id = response.headers.get(api::FILE_ID_HEADER).and_then(|v| v.parse().ok()).unwrap_or(0);
    PathingData::from_path_chunk(mapfile_id, file_id, &response.bytes).map_err(|e| format!("bad pathing data: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
mod local {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};

    use eframe::egui;
    use gw_nav::PathingStore;
    use gw_nav::api::MapEntry;
    use gw_nav::pathing::Progress;

    use super::Event;

    /// The map list from MapDb at `db`.
    pub fn map_list(db: &Path) -> Result<Vec<MapEntry>, String> {
        gw_nav::zones::map_list(db).map_err(|e| format!("{}: {e}", db.display()))
    }

    pub fn run(
        cache_dir: PathBuf,
        db: PathBuf,
        zones_db: Option<PathBuf>,
        requests: Receiver<(u32, bool)>,
        events: Sender<Event>,
        ctx: egui::Context,
    ) {
        let mut store = PathingStore::new(cache_dir);
        for (mapfile_id, refresh) in requests {
            let send = |e: Event| {
                let _ = events.send(e);
                ctx.request_repaint();
            };
            let progress = |p: Progress| {
                let fraction = |done: usize, total: usize| Some(done as f32 / total.max(1) as f32);
                let (text, fraction) = match p {
                    Progress::Connecting => ("Connecting to the fileserver".to_owned(), None),
                    Progress::Manifest => ("Downloading the asset manifest".to_owned(), None),
                    Progress::Map(done, total) => {
                        ("Downloading the map file".to_owned(), fraction(done as usize, total as usize))
                    }
                    Progress::Models(done, total) => {
                        (format!("Downloading models ({done}/{total})"), fraction(done, total))
                    }
                    Progress::MissingFile(id) => (format!("File {id} not found, ignored"), None),
                    Progress::Generating => ("Generating the pathing planes".to_owned(), None),
                    Progress::RenderFiles(done, total) => {
                        (format!("Downloading textures ({done}/{total})"), fraction(done, total))
                    }
                    Progress::Rendering => ("Rendering the map".to_owned(), None),
                    Progress::RenderFailed => ("Rendering the map failed".to_owned(), None),
                };
                send(Event::Progress(text, fraction));
            };
            send(Event::Progress(format!("Loading map file {mapfile_id}"), None));
            let result = if refresh { store.fetch(mapfile_id, progress) } else { store.load(mapfile_id, progress) };
            // A newly downloaded asset manifest may name map files MapDb has
            // no row for; they join the map list.
            match gw_nav::zones::record_manifest_mapfiles(&db, &store) {
                Ok(Some(new)) if new > 0 => send(Event::Maps(map_list(&db))),
                Ok(_) => {}
                Err(e) => send(Event::Progress(format!("Recording manifest map files failed: {e}"), None)),
            }
            match result {
                Ok(data) => {
                    let file_id = data.file_id;
                    send(Event::Loaded(Box::new(data)));
                    let a = gw_nav::zones::annotations(&mut store, &db, zones_db.as_deref(), mapfile_id, file_id);
                    send(Event::Annotations(mapfile_id, Box::new(a)));
                    let image = store
                        .load_render(mapfile_id, false, progress)
                        .map_err(|e| e.to_string())
                        .and_then(|(_, data)| super::decode_render(&data));
                    send(Event::Background(mapfile_id, image));
                }
                Err(e) => send(Event::Failed(mapfile_id, e.to_string())),
            }
        }
    }
}
