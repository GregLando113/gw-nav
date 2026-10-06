//! Reference map files for tests (gitignored game data in `testdata/`).
//!
//! For each base map file id:
//! - `<id>.mapblob`: stage 1, as served by the fileserver for the current
//!   revision (`cargo run -- download <id> --out-dir testdata`).
//! - `<id>.stage2.bin`: the same revision as bloated by the game client,
//!   taken from a Gw.dat (`cargo run --example dat_extract -- <Gw.dat> <id>
//!   testdata/<id>.stage2.bin`).
//!
//! Tests skip pairs whose files are missing.

use std::path::PathBuf;

use super::Ffna;

/// Base ids of the reference maps.
const MAP_IDS: &[u32] = &[
    290943, // Jaga Moraine
    288299, // Eye of the North / Ice Cliff Chasms
];

#[derive(Debug, Clone, Copy)]
pub struct MapPair {
    pub base_id: u32,
}

fn read(name: String) -> Option<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    std::fs::read(path).ok()
}

impl MapPair {
    pub fn all() -> impl Iterator<Item = MapPair> {
        MAP_IDS.iter().map(|&base_id| MapPair { base_id })
    }

    pub fn strip(&self) -> Option<Vec<u8>> {
        read(format!("{}.mapblob", self.base_id))
    }

    pub fn bloated(&self) -> Option<Vec<u8>> {
        read(format!("{}.stage2.bin", self.base_id))
    }

    /// The payloads of chunk `strip_id` in the stage-1 file and `bloated_id`
    /// in the stage-2 file, or `None` if either file is missing.
    pub fn chunks(&self, strip_id: u32, bloated_id: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        let (Some(strip), Some(bloated)) = (self.strip(), self.bloated()) else {
            eprintln!("{self:?}: reference files missing, skipped");
            return None;
        };
        let chunk = |data: &[u8], id| {
            Ffna::parse(data)
                .unwrap()
                .chunk(id)
                .unwrap_or_else(|| panic!("{self:?}: missing chunk {id:#x}"))
                .to_vec()
        };
        Some((chunk(&strip, strip_id), chunk(&bloated, bloated_id)))
    }
}
