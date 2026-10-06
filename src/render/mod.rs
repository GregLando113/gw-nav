//! Top-down render of a map, baked into a flat image while the pathing data
//! is generated (see `docs/world-render-scope.md`).
//!
//! Much of this is derived from Jonathan Bjørn Greve's GuildWarsMapBrowser
//! (<https://github.com/Jonathan-Greve/GuildWarsMapBrowser>); see the
//! module docs for the specific sources.
//!
//! Everything here is CPU-only so it runs wherever pathing is generated:
//! the desktop app's loader thread, the CLI and the headless relay.

pub mod atex;
pub mod bake;
pub mod canvas;
pub mod model;
pub mod props;
pub mod terrain;

pub use bake::{PropEntry, SpriteImage, WorldRender, bake, bake_with, required_files, required_model_textures};

/// Version of the baked render. Bumped whenever rendering changes, which
/// re-renders cached images (pathing data is versioned separately).
pub const RENDER_VERSION: u32 = 2;

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    #[error("texture: {0}")]
    Texture(&'static str),
    #[error("map file: {0}")]
    MapFile(#[from] crate::mapfile::MapFileError),
    #[error("image: {0}")]
    Image(String),
}

pub type Result<T> = std::result::Result<T, RenderError>;
