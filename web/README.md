# Web visualizer

The visualizer runs in the browser as WebAssembly (WebGPU, falling back to
WebGL). Browsers can't talk to the Guild Wars fileserver, so the `gw-nav-relay`
binary serves the page and its data: the MapDb list and pathing data, which
it takes from its cache or downloads and generates on demand, exactly like
the desktop app.

## One-time setup

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
```

The `wasm-bindgen-cli` version must match the `wasm-bindgen` version in
`Cargo.lock`.

## Build and run

```
web/build.ps1            # or web/build.sh; writes web/pkg
cargo run --release --bin gw-nav-relay
```

Then open <http://127.0.0.1:8080/>. `?map=<mapfile_id>` loads a map on
startup, e.g. <http://127.0.0.1:8080/?map=290943>.

## Headless (servers, containers)

The relay needs no desktop or GPU. Build it without the GUI stack (eframe,
wgpu, winit), which the default `gui` feature pulls in for the visualizer:

```
cargo build --release --bin gw-nav-relay --no-default-features
```

Build the web bundle (`web/pkg`) on a machine with the wasm tools and copy
`web/` next to the relay, or point `--web-dir` at it.

Relay options: `--addr` (default `127.0.0.1:8080`), `--db` (default
`mapfiles.db`), `--cache-dir` (default `cache`, shared with the CLI and the
desktop app), `--web-dir` (default `web`), `--zones-db` (gwbs `zones.db`
with recorded zone exits for the Zone exits layer; the desktop app takes the
same flag).

The desktop app can use a relay too:
`cargo run --release -- --relay http://127.0.0.1:8080`.

## API

See `src/api.rs`:

- `GET /api/maps`: MapDb rows as JSON.
- `GET /api/pathing/<mapfile_id>[?refresh=1]`: the stage-2 path chunk, with
  its revision in the `X-Gw-File-Id` header. Generating an uncached map
  takes a few seconds; requests for different maps are handled one at a
  time.
- `GET /api/annotations/<mapfile_id>`: the map file's mission points and
  portal props, and the zone exits recorded for its maps, as JSON. Available once the map's
  pathing data has been loaded.
