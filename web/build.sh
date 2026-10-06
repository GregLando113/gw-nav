#!/bin/sh
# Build the web visualizer into web/pkg (needs the wasm32-unknown-unknown
# target and wasm-bindgen-cli matching the wasm-bindgen version in Cargo.lock).
set -e
cd "$(dirname "$0")/.."
cargo build --release --target wasm32-unknown-unknown --bin gw-nav
wasm-bindgen --target web --no-typescript --out-name gw-nav --out-dir web/pkg target/wasm32-unknown-unknown/release/gw-nav.wasm
