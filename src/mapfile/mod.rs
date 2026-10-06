//! Map files (FFNA type 3) and the client's "bloat" of them.
//!
//! The fileserver serves map files in stage 1 (`MAP_STAGE_STRIP`, chunk ids
//! `0x10xxxxxx`/`0x11xxxxxx`). The client bloats every chunk into stage 2
//! (`0x20xxxxxx`/`0x21xxxxxx`), generating the pathing data along the way.
//! See `docs/mapblob-format.md`.

pub mod bits;
pub mod environment;
pub mod ffna;
pub mod huffman;
pub mod mission;
pub mod navmesh;
pub mod params;
pub mod path;
pub mod props;
pub mod tags;
pub mod terrain;
#[cfg(test)]
pub(crate) mod testdata;

pub use ffna::{Chunk, Ffna};

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum MapFileError {
    #[error("not an FFNA file")]
    NotFfna,
    #[error("data ends early at offset {offset}")]
    Truncated { offset: usize },
    #[error("expected tag {expected:#04x} at offset {offset}, found {found:#04x}")]
    UnexpectedTag { offset: usize, expected: u8, found: u8 },
    #[error("bad signature {found:#010x} (expected {expected:#010x})")]
    BadSignature { expected: u32, found: u32 },
    #[error("unsupported version {0}")]
    BadVersion(u32),
    #[error("missing chunk {0:#010x}")]
    MissingChunk(u32),
    #[error("invalid {0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, MapFileError>;

/// Map chunk kinds: the low byte of a chunk id. Names are the client's own,
/// from its map chunk descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChunkKind {
    Header = 0x00,
    EditorOld = 0x01,
    Terrain = 0x02,
    Zones = 0x03,
    Props = 0x04,
    Obsolete1 = 0x05,
    Water = 0x06,
    Mission = 0x07,
    Path = 0x08,
    Environment = 0x09,
    Locations = 0x0A,
    Obsolete2 = 0x0B,
    MapParameters = 0x0C,
    Editor = 0x0D,
    Collision = 0x0E,
    Light = 0x0F,
    Shore = 0x10,
    Sight = 0x11,
    Sound = 0x12,
    CubeMap = 0x13,
    VisData = 0x14,
    Occluders = 0x15,
    PathEngine = 0x16,
}

impl ChunkKind {
    const ALL: [ChunkKind; 23] = [
        Self::Header,
        Self::EditorOld,
        Self::Terrain,
        Self::Zones,
        Self::Props,
        Self::Obsolete1,
        Self::Water,
        Self::Mission,
        Self::Path,
        Self::Environment,
        Self::Locations,
        Self::Obsolete2,
        Self::MapParameters,
        Self::Editor,
        Self::Collision,
        Self::Light,
        Self::Shore,
        Self::Sight,
        Self::Sound,
        Self::CubeMap,
        Self::VisData,
        Self::Occluders,
        Self::PathEngine,
    ];

    pub fn from_id(chunk_id: u32) -> Option<Self> {
        Self::ALL.get((chunk_id & 0xFF) as usize).copied()
    }
}

/// Stage of a chunk id, from its top byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Stage 1 (`MAP_STAGE_STRIP`), as served by the fileserver.
    Strip,
    /// Stage 2, after the client's bloat.
    Bloated,
}

/// What a chunk id refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkId {
    pub stage: Stage,
    pub kind: ChunkKind,
    /// A file reference list (`0x11`/`0x21` ids) instead of chunk data.
    pub file_refs: bool,
}

impl ChunkId {
    pub fn new(stage: Stage, kind: ChunkKind, file_refs: bool) -> Self {
        Self { stage, kind, file_refs }
    }

    pub fn parse(id: u32) -> Option<Self> {
        let (stage, file_refs) = match id >> 24 {
            0x10 => (Stage::Strip, false),
            0x11 => (Stage::Strip, true),
            0x20 => (Stage::Bloated, false),
            0x21 => (Stage::Bloated, true),
            _ => return None,
        };
        if id & 0x00FF_FF00 != 0 {
            return None;
        }
        Some(Self { stage, kind: ChunkKind::from_id(id)?, file_refs })
    }

    pub fn id(self) -> u32 {
        let stage = match self.stage {
            Stage::Strip => 0x10,
            Stage::Bloated => 0x20,
        };
        (stage + self.file_refs as u32) << 24 | self.kind as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_ids() {
        let path = ChunkId::parse(0x1000_0008).unwrap();
        assert_eq!(path, ChunkId::new(Stage::Strip, ChunkKind::Path, false));
        assert_eq!(ChunkId::new(Stage::Bloated, ChunkKind::Path, false).id(), 0x2000_0008);
        assert_eq!(ChunkId::new(Stage::Bloated, ChunkKind::Props, true).id(), 0x2100_0004);
        assert_eq!(ChunkId::parse(0x2100_0016).unwrap().kind, ChunkKind::PathEngine);
        assert_eq!(ChunkId::parse(0x1000_0017), None);
        assert_eq!(ChunkId::parse(0x3000_0008), None);
        assert_eq!(ChunkId::parse(0x1001_0008), None);
    }
}

/// Signature of the file reference lists in `0x11xxxxxx` / `0x21xxxxxx`
/// chunks.
pub const FILE_REFS_SIGNATURE: u32 = 0x2993_9830;

/// Parse a file reference list: `u32 0x29939830, u8 1`, then 6-byte
/// entries `(u16 w0, u16 w1, u16 0)`. Each entry is a file name hash
/// (a 2-character wide string) that decodes to the base file id
/// `(w0 - 0xFF00FF) + w1 * 0xFF00`.
pub fn parse_file_refs(data: &[u8]) -> Result<Vec<u32>> {
    let mut r = tags::Reader::new(data);
    let signature = r.u32()?;
    if signature != FILE_REFS_SIGNATURE {
        return Err(MapFileError::BadSignature { expected: FILE_REFS_SIGNATURE, found: signature });
    }
    r.u8()?;
    let mut ids = Vec::new();
    while !r.is_empty() {
        let (w0, w1) = (r.u16()? as u32, r.u16()? as u32);
        r.u16()?;
        ids.push(w0.wrapping_sub(0xFF_00FF).wrapping_add(w1.wrapping_mul(0xFF00)));
    }
    Ok(ids)
}
