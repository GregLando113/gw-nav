//! Tagged chunk payloads (the client's `Engine\Map\Services\MsChunk.cpp`).
//!
//! Map chunk payloads are a sequence of tagged sections. How a tag is
//! encoded depends on the stage:
//! - stage 1 (`MAP_STAGE_STRIP`, as served by the fileserver): a bare `u8`
//!   tag; the section's length is implied by its contents.
//! - stage 2 (bloated): `u8 tag, u32 len`.

use super::{MapFileError, Result};

/// Tag that ends a payload in both stages.
pub const TAG_END: u8 = 0xFF;

/// Little-endian cursor over a chunk payload.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or(MapFileError::Truncated { offset: self.pos })?;
        self.pos += n;
        Ok(out)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    pub fn peek_u8(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// Expect a stage-1 tag (a bare byte).
    pub fn strip_tag(&mut self, tag: u8) -> Result<()> {
        let offset = self.pos;
        let found = self.u8()?;
        if found != tag {
            return Err(MapFileError::UnexpectedTag { offset, expected: tag, found });
        }
        Ok(())
    }

    /// Expect a stage-2 tag and return its section.
    pub fn tag(&mut self, tag: u8) -> Result<&'a [u8]> {
        let offset = self.pos;
        let found = self.u8()?;
        if found != tag {
            return Err(MapFileError::UnexpectedTag { offset, expected: tag, found });
        }
        let len = self.u32()? as usize;
        self.bytes(len)
    }

    /// Read the next stage-2 section, whatever its tag.
    pub fn any_tag(&mut self) -> Result<(u8, &'a [u8])> {
        let tag = self.u8()?;
        let len = self.u32()? as usize;
        Ok((tag, self.bytes(len)?))
    }
}

/// Builds a stage-2 chunk payload.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

/// Position of a stage-2 section length that is patched when the section
/// ends (the client's `Chunk_finalize_tag_size`).
#[must_use = "an open section must be closed with Writer::end_tag"]
#[derive(Debug)]
pub struct OpenTag(usize);

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&mut self, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(data);
        self
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.bytes(&[v])
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    /// Write a stage-2 section whose length is known up front.
    pub fn tag(&mut self, tag: u8, data: &[u8]) -> &mut Self {
        self.u8(tag).u32(data.len() as u32).bytes(data)
    }

    /// Start a stage-2 section whose length is filled in by `end_tag`.
    pub fn begin_tag(&mut self, tag: u8) -> OpenTag {
        self.u8(tag).u32(0);
        OpenTag(self.buf.len() - 4)
    }

    pub fn end_tag(&mut self, open: OpenTag) {
        let len = (self.buf.len() - open.0 - 4) as u32;
        self.buf[open.0..open.0 + 4].copy_from_slice(&len.to_le_bytes());
    }

    /// Bytes written since `open` began, excluding its header.
    pub fn since(&self, open: &OpenTag) -> &[u8] {
        &self.buf[open.0 + 4..]
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_primitives() {
        let data = [7, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12, 0, 0, 0x80, 0x3f];
        let mut r = Reader::new(&data);
        r.strip_tag(7).unwrap();
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.u32().unwrap(), 0x12345678);
        assert_eq!(r.f32().unwrap(), 1.0);
        assert!(r.is_empty());
        assert_eq!(r.u8(), Err(MapFileError::Truncated { offset: 11 }));
    }

    #[test]
    fn strip_tag_mismatch() {
        let mut r = Reader::new(&[8]);
        assert_eq!(
            r.strip_tag(7),
            Err(MapFileError::UnexpectedTag { offset: 0, expected: 7, found: 8 })
        );
    }

    #[test]
    fn stage2_sections_roundtrip() {
        let mut w = Writer::new();
        w.tag(7, &[1, 2]);
        let open = w.begin_tag(8);
        w.u32(0xAABBCCDD).u8(9);
        assert_eq!(w.since(&open), &[0xDD, 0xCC, 0xBB, 0xAA, 9]);
        w.end_tag(open);
        w.u8(TAG_END);

        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.tag(7).unwrap(), &[1, 2]);
        assert_eq!(r.any_tag().unwrap(), (8, &[0xDD, 0xCC, 0xBB, 0xAA, 9][..]));
        r.strip_tag(TAG_END).unwrap();
        assert!(r.is_empty());
    }
}
