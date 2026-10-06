//! Dev tool: extract a file from a local Gw.dat by file id.
//!
//! The app never reads Gw.dat. This exists only to pull the client's bloated
//! (stage-2) copy of a map file as a reference to test our own bloat against.
//!
//!     cargo run --example dat_extract -- <Gw.dat> <file_id> [out_path]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail};
use gw_nav::fileconn::decompress;

/// Size of one MFT entry. Entry 0 is the MFT header, 1 the dat header,
/// 2 the hash list and 3 the MFT itself.
const MFT_ENTRY_SIZE: u64 = 24;
/// MFT index of the (file id, MFT index) hash list.
const HASH_LIST_INDEX: u64 = 2;

struct MftEntry {
    offset: u64,
    size: u32,
    compressed: bool,
}

fn read_at(dat: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    dat.seek(SeekFrom::Start(offset))?;
    dat.read_exact(&mut buf)?;
    Ok(buf)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn mft_entry(dat: &mut File, mft_offset: u64, index: u64) -> Result<MftEntry> {
    let e = read_at(dat, mft_offset + index * MFT_ENTRY_SIZE, MFT_ENTRY_SIZE as usize)?;
    Ok(MftEntry {
        offset: u64::from_le_bytes(e[0..8].try_into().unwrap()),
        size: u32_at(&e, 8),
        compressed: u16::from_le_bytes([e[12], e[13]]) != 0,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        bail!("usage: dat_extract <Gw.dat> <file_id> [out_path]");
    }
    let file_id: u32 = args[2].parse().context("file_id must be a number")?;
    let out_path = args.get(3).cloned().unwrap_or_else(|| format!("{file_id}.dat.bin"));

    let mut dat = File::open(&args[1]).with_context(|| format!("opening {}", args[1]))?;
    let header = read_at(&mut dat, 0, 32)?;
    if &header[0..4] != b"3AN\x1a" {
        bail!("not a Gw.dat file");
    }
    let mft_offset = u64::from_le_bytes(header[16..24].try_into().unwrap());

    let hash_list = mft_entry(&mut dat, mft_offset, HASH_LIST_INDEX)?;
    let hashes = read_at(&mut dat, hash_list.offset, hash_list.size as usize)?;
    let index = hashes
        .chunks_exact(8)
        .find(|h| u32_at(h, 0) == file_id)
        .map(|h| u32_at(h, 4))
        .with_context(|| format!("file id {file_id} not in Gw.dat"))?;

    let entry = mft_entry(&mut dat, mft_offset, index as u64)?;
    let raw = read_at(&mut dat, entry.offset, entry.size as usize)?;
    let data = if entry.compressed {
        // The decompressed size is stored in the last word of the stream.
        let out_size = u32_at(&raw, raw.len() - 4) as usize;
        decompress(&raw, out_size).context("decompressing")?
    } else {
        raw
    };
    std::fs::write(&out_path, &data)?;
    println!(
        "{file_id}: mft #{index}, {} bytes -> {} bytes, saved {out_path}",
        entry.size,
        data.len()
    );
    Ok(())
}
