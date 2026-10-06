//! Guild Wars Navigator: browse MapDb, load a map's pathing data on demand
//! and view its planes; build waypoint lists on it.
//!
//! Native: `gw-nav [--relay URL] [mapfile_id]`, loading locally unless a
//! relay is given. Web: built for wasm32 and served by the `gw-nav-relay` binary,
//! which it also gets its data from (see `web/README.md`).

// Proving that wgpu's resources are `Send` (for egui-wgpu's callback
// resources) exceeds the default limit on current nightlies.
#![recursion_limit = "256"]

mod annotations;
mod app;
mod clipboard;
mod render;
mod source;
mod waypoints;
mod zone_chunk;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    use clap::Parser;
    use eframe::egui;

    #[derive(Parser)]
    #[command(about = "Guild Wars Navigator: pathing map visualizer")]
    struct Cli {
        /// Path to the MapDb sqlite file.
        #[arg(long, default_value = "mapfiles.db")]
        db: std::path::PathBuf,
        /// Cache directory for downloads and generated pathing data.
        #[arg(long, default_value = "cache")]
        cache_dir: std::path::PathBuf,
        /// gwbs zones.db with recorded zone exits (table `zone_exit`).
        #[arg(long)]
        zones_db: Option<std::path::PathBuf>,
        /// Get data from a relay server (e.g. http://127.0.0.1:8080) instead
        /// of loading locally.
        #[arg(long)]
        relay: Option<String>,
        /// Map file id to load on startup.
        mapfile_id: Option<u32>,
    }

    let cli = Cli::parse();
    let source = match cli.relay {
        Some(url) => app::SourceKind::Relay(url),
        None => app::SourceKind::Local { db: cli.db, cache_dir: cli.cache_dir, zones_db: cli.zones_db },
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 900.0]).with_title("Guild Wars Navigator"),
        ..Default::default()
    };
    eframe::run_native(
        "gw-nav",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, app::Options { source, initial_mapfile: cli.mapfile_id })))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    let window = web_sys::window().expect("no window");
    // The relay serves this page, so its API is on the same origin.
    let origin = window.location().origin().unwrap_or_else(|_| gw_nav::api::DEFAULT_RELAY.to_owned());
    // `?map=<mapfile_id>` loads a map on startup.
    let search = window.location().search().unwrap_or_default();
    let initial_mapfile = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("map=")?.parse().ok());
    let canvas = window
        .document()
        .and_then(|d| d.get_element_by_id("visualizer"))
        .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        .expect("no <canvas id=\"visualizer\">");
    wasm_bindgen_futures::spawn_local(async move {
        let options = app::Options { source: app::SourceKind::Relay(origin), initial_mapfile };
        let result = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(move |cc| Ok(Box::new(app::App::new(cc, options)))))
            .await;
        if let Err(e) = result {
            web_sys::console::error_1(&e);
        }
    });
}
