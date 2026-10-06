pub mod api;
#[cfg(not(target_arch = "wasm32"))]
pub use gw_fileconn as fileconn;
#[cfg(not(target_arch = "wasm32"))]
pub mod mapdb;
pub mod mapfile;
pub mod pathfind;
pub mod pathgen;
pub mod pathing;
pub mod render;
pub mod waypoint;
#[cfg(not(target_arch = "wasm32"))]
pub mod zones;

#[cfg(not(target_arch = "wasm32"))]
pub use fileconn::{AssetManifest, FileClient, FileConnError, RawFile};
#[cfg(not(target_arch = "wasm32"))]
pub use mapdb::{ImportReport, MapDb, MapDbError, MapZone};
pub use pathing::PathingData;
#[cfg(not(target_arch = "wasm32"))]
pub use pathing::{PathingError, PathingStore};
