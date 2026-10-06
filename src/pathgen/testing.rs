//! Shared fixtures for path generation tests: the reference maps' stage-1
//! inputs, the client's stage-2 outputs and our tracer output.

use crate::mapfile::navmesh::{PathPlane, parse_planes};
use crate::mapfile::params::MapParams;
use crate::mapfile::path::{PathBloated, PathStrip};
use crate::mapfile::props::PropsCollision;
use crate::mapfile::terrain::TerrainStrip;
use crate::mapfile::testdata::MapPair;
use crate::mapfile::Ffna;

use super::chunk::trace_terrain;
use super::tracer::TracerOutput;

#[allow(dead_code)]
pub struct Fixture {
    pub pair: MapPair,
    pub start_points: Vec<[f32; 2]>,
    /// The client's props collision (identical to ours, see
    /// `pathgen::props`), so tests don't need the model files.
    pub props: PropsCollision,
    pub traced: TracerOutput,
    pub client_path: Vec<u8>,
    pub client_planes: Vec<PathPlane>,
}

impl Fixture {
    pub fn all() -> impl Iterator<Item = Fixture> {
        MapPair::all().filter_map(Fixture::load)
    }

    pub fn load(pair: MapPair) -> Option<Fixture> {
        let (strip, bloated) = (pair.strip()?, pair.bloated()?);
        let strip = Ffna::parse(&strip).unwrap();
        let bloated = Ffna::parse(&bloated).unwrap();
        let terrain = TerrainStrip::parse(strip.chunk(0x1000_0002).unwrap()).unwrap();
        let path = PathStrip::parse(strip.chunk(0x1000_0008).unwrap()).unwrap();
        let params = MapParams::parse(strip.chunk(0x1000_000C).unwrap()).unwrap();
        let props = PropsCollision::parse_bloated(bloated.chunk(0x2000_0004).unwrap()).unwrap();
        let traced = trace_terrain(&terrain, &params, &path, &props);
        let client_path = bloated.chunk(0x2000_0008).unwrap().to_vec();
        let client_planes = parse_planes(PathBloated::parse(&client_path).unwrap().planes).unwrap();
        Some(Fixture { pair, start_points: path.start_points, props, traced, client_path, client_planes })
    }
}
