//! Dev tool: decode Guild Wars texture files (`ATEX`/`ATTX`) to PNG, to
//! check the decoder by eye.
//!
//!     cargo run --example texture_png -- <texture file>... --out-dir <dir>

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use gw_nav::render::atex;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let Some(flag) = args.iter().position(|a| a == "--out-dir") else { bail!("usage: <file>... --out-dir <dir>") };
    let out_dir = PathBuf::from(args.get(flag + 1).context("missing --out-dir value")?);
    args.drain(flag..flag + 2);
    std::fs::create_dir_all(&out_dir)?;
    for path in args {
        let data = std::fs::read(&path).with_context(|| path.clone())?;
        match atex::decode(&data) {
            Ok(t) => {
                let rgba: Vec<u8> = t.pixels.iter().flatten().copied().collect();
                let name = PathBuf::from(&path).file_stem().unwrap().to_string_lossy().into_owned();
                let out = out_dir.join(format!("{name}.png"));
                image::save_buffer(&out, &rgba, t.width as u32, t.height as u32, image::ColorType::Rgba8)?;
                println!("{path}: {}x{} -> {}", t.width, t.height, out.display());
            }
            Err(e) => println!("{path}: {e}"),
        }
    }
    Ok(())
}
