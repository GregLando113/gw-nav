//! FFNA container: `"ffna"`, a type byte, then `(u32 id, u32 len, data)`
//! chunks until the end of the file.

use super::{MapFileError, Result};

pub const MAGIC: &[u8; 4] = b"ffna";
/// FFNA type byte of map files.
pub const TYPE_MAP: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    pub id: u32,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ffna<'a> {
    pub file_type: u8,
    pub chunks: Vec<Chunk<'a>>,
}

impl<'a> Ffna<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        if data.len() < 5 || &data[..4] != MAGIC {
            return Err(MapFileError::NotFfna);
        }
        let file_type = data[4];
        let mut chunks = Vec::new();
        let mut pos = 5;
        while pos < data.len() {
            let header = data
                .get(pos..pos + 8)
                .ok_or(MapFileError::Truncated { offset: pos })?;
            let id = u32::from_le_bytes(header[..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            let start = pos + 8;
            let data = data
                .get(start..start + len)
                .ok_or(MapFileError::Truncated { offset: pos })?;
            chunks.push(Chunk { id, data });
            pos = start + len;
        }
        Ok(Self { file_type, chunks })
    }

    /// First chunk with `id`.
    pub fn chunk(&self, id: u32) -> Option<&'a [u8]> {
        self.chunks.iter().find(|c| c.id == id).map(|c| c.data)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let size = 5 + self.chunks.iter().map(|c| 8 + c.data.len()).sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(MAGIC);
        out.push(self.file_type);
        for chunk in &self.chunks {
            out.extend_from_slice(&chunk.id.to_le_bytes());
            out.extend_from_slice(&(chunk.data.len() as u32).to_le_bytes());
            out.extend_from_slice(chunk.data);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let file = Ffna {
            file_type: TYPE_MAP,
            chunks: vec![
                Chunk { id: 0x10000000, data: &[1, 2, 3] },
                Chunk { id: 0x10000008, data: &[] },
            ],
        };
        let bytes = file.to_bytes();
        assert_eq!(&bytes[..5], b"ffna\x03");
        assert_eq!(Ffna::parse(&bytes).unwrap(), file);
        assert_eq!(file.chunk(0x10000000), Some(&[1u8, 2, 3][..]));
        assert_eq!(file.chunk(0x10000001), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Ffna::parse(b"ffn"), Err(MapFileError::NotFfna));
        assert_eq!(Ffna::parse(b"abcd\x03"), Err(MapFileError::NotFfna));
        // Chunk claims 4 bytes but only 1 follows.
        let bad = [b"ffna\x03".as_slice(), &[8, 0, 0, 0x10, 4, 0, 0, 0, 9]].concat();
        assert_eq!(Ffna::parse(&bad), Err(MapFileError::Truncated { offset: 5 }));
    }
}
