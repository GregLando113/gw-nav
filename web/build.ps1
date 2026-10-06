# Build the web visualizer into web/pkg (needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli matching the wasm-bindgen version in Cargo.lock).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
cargo build --release --target wasm32-unknown-unknown --bin gw-nav
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
wasm-bindgen --target web --no-typescript --out-name gw-nav --out-dir web/pkg target/wasm32-unknown-unknown/release/gw-nav.wasm
exit $LASTEXITCODE
