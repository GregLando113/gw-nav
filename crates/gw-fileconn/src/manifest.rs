//! The fileserver's asset manifest.
//!
//! File ids used by the game protocol and by MapDb (e.g. the map file id in
//! the SpawnInfo packet) are *base* ids that never change. The fileserver
//! keeps serving the original revision under a base id, so the current
//! revision must be looked up in the asset manifest, whose own file id is
//! announced in the server hello (`FileClient::manifest()[1]`).
//!
//! Format: a leading `u32 0`, then records of little-endian u32s, each
//! terminated by a 0: `current_id, base_id, dependency_base_ids...`.

use std::collections::{HashMap, HashSet};

/// Fewest dependencies of a [`AssetManifest::map_file_candidates`] entry.
/// The smallest known map file has 74.
pub const MIN_MAP_DEPENDENCIES: usize = 50;

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest length {0} is not a multiple of 4")]
    BadLength(usize),
    #[error("manifest does not start with a 0 word")]
    BadHeader,
    #[error("manifest record at word {0} has fewer than 2 ids")]
    ShortRecord(usize),
    #[error("manifest ends without a record terminator")]
    Unterminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// File id of the current revision, the one to download.
    pub current_id: u32,
    /// Base ids of the files this file depends on (textures, models, ...).
    pub dependencies: Vec<u32>,
}

/// A manifest entry that looks like a map file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapFileCandidate {
    pub base_id: u32,
    pub current_id: u32,
    pub dependencies: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AssetManifest {
    entries: HashMap<u32, ManifestEntry>,
}

impl AssetManifest {
    pub fn parse(data: &[u8]) -> Result<Self, ManifestError> {
        if !data.len().is_multiple_of(4) {
            return Err(ManifestError::BadLength(data.len()));
        }
        let words: Vec<u32> = data
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        if words.first() != Some(&0) {
            return Err(ManifestError::BadHeader);
        }

        let mut entries = HashMap::new();
        let mut start = 1;
        for (i, &word) in words.iter().enumerate().skip(1) {
            if word != 0 {
                continue;
            }
            let record = &words[start..i];
            let [current_id, base_id, dependencies @ ..] = record else {
                return Err(ManifestError::ShortRecord(start));
            };
            entries.insert(
                *base_id,
                ManifestEntry {
                    current_id: *current_id,
                    dependencies: dependencies.to_vec(),
                },
            );
            start = i + 1;
        }
        if start != words.len() {
            return Err(ManifestError::Unterminated);
        }
        Ok(Self { entries })
    }

    pub fn get(&self, base_id: u32) -> Option<&ManifestEntry> {
        self.entries.get(&base_id)
    }

    /// Current file id for `base_id`, or `base_id` itself if it isn't listed.
    pub fn resolve(&self, base_id: u32) -> u32 {
        self.get(base_id).map_or(base_id, |e| e.current_id)
    }

    /// The entries that look like map files, by base id. The manifest has no
    /// file types, but map files stand out: no file depends on them, and
    /// they depend on many (their models and textures). In manifest 390279
    /// all 246 map files known to MapDb have at least 74 dependencies, and
    /// 370 roots have at least [`MIN_MAP_DEPENDENCIES`]. Other large roots
    /// can slip in, so a candidate is only a map once its file says so.
    pub fn map_file_candidates(&self) -> Vec<MapFileCandidate> {
        let depended: HashSet<u32> = self.entries.values().flat_map(|e| e.dependencies.iter().copied()).collect();
        let mut candidates: Vec<MapFileCandidate> = self
            .entries
            .iter()
            .filter(|(base, e)| e.dependencies.len() >= MIN_MAP_DEPENDENCIES && !depended.contains(base))
            .map(|(&base_id, e)| MapFileCandidate {
                base_id,
                current_id: e.current_id,
                dependencies: e.dependencies.len(),
            })
            .collect();
        candidates.sort_by_key(|c| c.base_id);
        candidates
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn parses_records() {
        let data = bytes(&[0, 0x2005, 0x2005, 0, 380894, 288299, 10052, 10054, 0]);
        let m = AssetManifest::parse(&data).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m.resolve(288299), 380894);
        assert_eq!(m.get(288299).unwrap().dependencies, vec![10052, 10054]);
        assert_eq!(m.resolve(0x2005), 0x2005);
        assert_eq!(m.resolve(12345), 12345);
    }

    #[test]
    fn finds_map_file_candidates() {
        // 100 depends on 60 leaves and model 200; 300 has few dependencies;
        // 200 has many but 100 depends on it.
        let leaves: Vec<u32> = (1000..1060).collect();
        let mut words = vec![0, 101, 100];
        words.extend(&leaves);
        words.extend([200, 0, 201, 200]);
        words.extend(&leaves);
        words.extend([0, 301, 300, 1000, 1001, 0]);
        let m = AssetManifest::parse(&bytes(&words)).unwrap();
        assert_eq!(
            m.map_file_candidates(),
            vec![MapFileCandidate { base_id: 100, current_id: 101, dependencies: 61 }]
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(AssetManifest::parse(&[0; 3]).unwrap_err(), ManifestError::BadLength(3));
        assert_eq!(AssetManifest::parse(&bytes(&[1, 2, 3, 0])).unwrap_err(), ManifestError::BadHeader);
        assert_eq!(AssetManifest::parse(&bytes(&[0, 5, 0])).unwrap_err(), ManifestError::ShortRecord(1));
        assert_eq!(AssetManifest::parse(&bytes(&[0, 5, 6])).unwrap_err(), ManifestError::Unterminated);
    }
}
