//! On-demand pathing data: download a map file and its prop models from the
//! fileserver, generate the stage-2 path chunk the way the client does
//! (`pathgen`), and cache the result keyed by map file id.
//!
//! [`PathingData`] (decoding a stage-2 path chunk) works on every target;
//! the downloading and caching [`PathingStore`] is native only (the web
//! build gets path chunks from the relay server instead).
//!
//! Cache layout under the cache directory:
//! - `manifest-<id>.bin`: the asset manifest (its id changes with updates)
//! - `files/<file_id>.bin`: decompressed downloads, keyed by the exact
//!   (current revision) file id
//! - `pathing/<mapfile_id>-r<file_id>-v<FORMAT_VERSION>.path`: the
//!   generated stage-2 path chunk (`0x20000008` payload)
//! - `render/<mapfile_id>-r<file_id>-v<RENDER_VERSION>.gwri`: the map's
//!   baked top-down render ([`crate::render::WorldRender`]), made along with
//!   the path chunk

use crate::mapfile::MapFileError;
use crate::mapfile::navmesh::{PathPlane, parse_planes};
use crate::mapfile::path::PathBloated;
use crate::pathgen::obstacles::{self, Obstacle};

#[cfg(not(target_arch = "wasm32"))]
mod store;
#[cfg(not(target_arch = "wasm32"))]
pub use store::{PathingError, PathingStore, Progress};

/// Version of the generated data. Bumped whenever generation changes, which
/// invalidates older cache entries.
pub const FORMAT_VERSION: u32 = 1;

/// The pathing data of one map.
#[derive(Debug, Clone, PartialEq)]
pub struct PathingData {
    /// Base map file id (as in MapDb).
    pub mapfile_id: u32,
    /// File id of the revision the data was generated from.
    pub file_id: u32,
    pub start_points: Vec<[f32; 2]>,
    pub planes: Vec<PathPlane>,
    /// The prop owning each plane (0 for the ground plane).
    pub plane_props: Vec<u16>,
    pub obstacles: Vec<Obstacle>,
}

impl PathingData {
    /// Decode a stage-2 path chunk payload.
    pub fn from_path_chunk(mapfile_id: u32, file_id: u32, data: &[u8]) -> Result<Self, MapFileError> {
        let path = PathBloated::parse(data)?;
        let (_, _, cells) = obstacles::parse(path.obstacles)?;
        let mut unique: Vec<Obstacle> = Vec::new();
        for o in cells.iter().flatten() {
            if !unique.contains(o) {
                unique.push(*o);
            }
        }
        Ok(Self {
            mapfile_id,
            file_id,
            start_points: path.start_points,
            planes: parse_planes(path.planes)?,
            plane_props: path.plane_indices,
            obstacles: unique,
        })
    }

    pub fn trapezoid_count(&self) -> usize {
        self.planes.iter().map(|p| p.trapezoids.len()).sum()
    }
}

