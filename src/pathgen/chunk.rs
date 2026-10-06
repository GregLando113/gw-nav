//! The whole path bloat (`PathStatic_Import`): stage-1 terrain, params,
//! path and props collision in, stage-2 path chunk (`0x20000008`) out.

use std::collections::HashMap;

use crate::mapfile::navmesh::{PathPlane, write_planes};
use crate::mapfile::params::MapParams;
use crate::mapfile::path::{PathStrip, write_bloated};
use crate::mapfile::props::{PropsCollision, PropsStrip};
use crate::mapfile::terrain::TerrainStrip;
use crate::mapfile::{Ffna, MapFileError, parse_file_refs};

use super::X87;
use super::assemble::assemble;
use super::build::{PortalRecords, build_plane};
use super::obstacles::{self, Obstacle};
use super::props::{ModelPoint, build_collision, model_collision};
use super::tracer::{TracerInput, TracerOutput, trace};

/// The parts of a stage-1 map file that path generation reads.
#[derive(Debug, Clone)]
pub struct MapInputs {
    pub terrain: TerrainStrip,
    pub params: MapParams,
    pub path: PathStrip,
    pub props: PropsStrip,
    /// Props file references (`0x11000004`): base file ids of the models.
    pub model_refs: Vec<u32>,
}

impl MapInputs {
    pub fn parse(map: &[u8]) -> Result<Self, MapFileError> {
        let file = Ffna::parse(map)?;
        let chunk = |id: u32| file.chunk(id).ok_or(MapFileError::MissingChunk(id));
        Ok(Self {
            terrain: TerrainStrip::parse(chunk(0x1000_0002)?)?,
            params: MapParams::parse(chunk(0x1000_000C)?)?,
            path: PathStrip::parse(chunk(0x1000_0008)?)?,
            props: PropsStrip::parse(chunk(0x1000_0004)?)?,
            model_refs: match file.chunk(0x1100_0004) {
                Some(refs) => parse_file_refs(refs)?,
                None => Vec::new(),
            },
        })
    }

    /// Base file ids of the models whose collision outlines are needed
    /// (props without flag bit 0), deduplicated, in reference order.
    pub fn required_models(&self) -> Vec<u32> {
        let mut used = vec![false; self.model_refs.len()];
        for p in self.props.props.iter().filter(|p| p.flags & 1 == 0) {
            if let Some(u) = used.get_mut(p.model as usize) {
                *u = true;
            }
        }
        let mut ids: Vec<u32> = self.model_refs.iter().zip(used).filter(|(_, u)| *u).map(|(id, _)| *id).collect();
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(*id));
        ids
    }

    pub fn bounds(&self) -> [f32; 4] {
        [self.params.min_x, self.params.min_y, self.params.max_x, self.params.max_y]
    }

    /// The props collision, given the model files by base id. Missing models
    /// count as having no collision outline.
    pub fn collision(&self, models: &HashMap<u32, Vec<u8>>) -> Result<PropsCollision, MapFileError> {
        let mut outlines: HashMap<u32, Vec<ModelPoint>> = HashMap::new();
        for (id, data) in models {
            outlines.insert(*id, model_collision(data)?);
        }
        let get = |m: u16| self.model_refs.get(m as usize).and_then(|id| outlines.get(id)).map(Vec::as_slice);
        Ok(build_collision(&self.props.props, get, X87::Double))
    }

    /// Generate the stage-2 path chunk. `obstacles` are the zone obstacles
    /// (path tag 13).
    pub fn bloat_path(&self, collision: &PropsCollision, obstacles: &[Obstacle]) -> Vec<u8> {
        let obstacles = obstacles::export(self.bounds(), obstacles);
        bloat_path(&self.terrain, &self.params, &self.path, collision, &obstacles)
    }
}

/// Trace the walkable outline of the terrain (the ground plane's input).
pub fn trace_terrain(
    terrain: &TerrainStrip,
    params: &MapParams,
    path: &PathStrip,
    collision: &PropsCollision,
) -> TracerOutput {
    let [dx, dy] = terrain.header.dims;
    let heights: Vec<f32> =
        (0..dy).flat_map(|y| (0..dx).map(move |x| (x, y))).map(|(x, y)| terrain.height(x, y)).collect();
    trace(&TracerInput {
        map_type: params.map_type(),
        water: params.has_water(),
        dims: terrain.header.dims,
        heights: &heights,
        min_x: params.min_x,
        max_y: params.max_y,
        start_points: &path.start_points,
        portal_points: &collision.portal_points,
        x87: X87::Double,
    })
}

/// Build every plane from the traced terrain and the props collision
/// (`PathChunk_BuildPathData`, `PathData_build_maps`).
pub fn build_planes(traced: &TracerOutput, path: &PathStrip, collision: &PropsCollision) -> Vec<PathPlane> {
    let mut records = PortalRecords::new();
    assemble(&traced.segments, &path.start_points, &collision.points)
        .iter()
        .enumerate()
        .map(|(i, input)| build_plane(i as u16, input, &mut records))
        .collect()
}

/// Generate the stage-2 path chunk. `collision` is the props collision
/// (`pathgen::props::build_collision`); `obstacles` is the tag 13 payload,
/// which is not generated yet (it needs the zone placements).
pub fn bloat_path(
    terrain: &TerrainStrip,
    params: &MapParams,
    path: &PathStrip,
    collision: &PropsCollision,
    obstacles: &[u8],
) -> Vec<u8> {
    let traced = trace_terrain(terrain, params, path, collision);
    let planes = build_planes(&traced, path, collision);
    write_bloated(path, &write_planes(&planes), &collision.plane_props, obstacles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::Ffna;
    use crate::mapfile::path::PathBloated;
    use crate::pathgen::testing::Fixture;

    /// The whole on-demand pipeline: the stage-1 file and the model files
    /// give the client's planes and plane props.
    #[test]
    fn map_inputs_generate_client_planes() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/models");
        for f in Fixture::all() {
            let inputs = MapInputs::parse(&f.pair.strip().unwrap()).unwrap();
            let required = inputs.required_models();
            let models: HashMap<u32, Vec<u8>> = required
                .iter()
                .filter_map(|id| Some((*id, std::fs::read(dir.join(format!("{id}.ffna"))).ok()?)))
                .collect();
            if models.len() != required.len() {
                eprintln!("{:?}: {} of {} models in testdata/models, skipped", f.pair, models.len(), required.len());
                continue;
            }
            let collision = inputs.collision(&models).unwrap();
            assert!(collision == f.props, "{:?}: props collision", f.pair);
            let ours = inputs.bloat_path(&collision, &[]);
            let ours = PathBloated::parse(&ours).unwrap();
            let client = PathBloated::parse(&f.client_path).unwrap();
            assert!(ours.planes == client.planes, "{:?}: planes", f.pair);
            assert_eq!(ours.plane_indices, client.plane_indices, "{:?}", f.pair);
            eprintln!("{:?}: {} models, planes equal", f.pair, required.len());
        }
    }

    /// The generated chunk equals the client's byte for byte (given the
    /// client's obstacles, which aren't generated yet).
    #[test]
    fn path_chunk_matches_client() {
        for f in Fixture::all() {
            let data = f.pair.strip().unwrap();
            let strip = Ffna::parse(&data).unwrap();
            let terrain = TerrainStrip::parse(strip.chunk(0x1000_0002).unwrap()).unwrap();
            let path = PathStrip::parse(strip.chunk(0x1000_0008).unwrap()).unwrap();
            let params = MapParams::parse(strip.chunk(0x1000_000C).unwrap()).unwrap();
            let obstacles = PathBloated::parse(&f.client_path).unwrap().obstacles;
            let ours = bloat_path(&terrain, &params, &path, &f.props, obstacles);
            assert_eq!(ours.len(), f.client_path.len(), "{:?}", f.pair);
            assert!(ours == f.client_path, "{:?}", f.pair);
            eprintln!("{:?}: path chunk equal ({} bytes)", f.pair, ours.len());
        }
    }
}
