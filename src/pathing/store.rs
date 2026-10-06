//! The native side of on-demand pathing: fileserver downloads, generation
//! and the on-disk cache (see the module docs of [`super`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fileconn::{AssetManifest, ConnectionPool, FileConnError};
use crate::mapfile::MapFileError;
use crate::pathgen::chunk::MapInputs;
use crate::render::{self, RENDER_VERSION, RenderError};

use super::{FORMAT_VERSION, PathingData};

/// Connections used to download a map's models and textures, and the size
/// of a store's own connection pool.
const MODEL_CONNECTIONS: usize = 4;
/// Times a download is retried on a fresh connection after an I/O error.
const RETRIES: usize = 2;

#[derive(thiserror::Error, Debug)]
pub enum PathingError {
    #[error(transparent)]
    FileConn(#[from] FileConnError),
    #[error("map file: {0}")]
    MapFile(#[from] MapFileError),
    #[error("render: {0}")]
    Render(#[from] RenderError),
    #[error("cache i/o on {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

pub type Result<T> = std::result::Result<T, PathingError>;

/// What [`PathingStore`] is doing, for progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Connecting,
    Manifest,
    /// Downloading the map file: bytes done / total.
    Map(u32, u32),
    /// Downloading models: models done / total.
    Models(usize, usize),
    /// A referenced model or texture is not on the fileserver. A model is
    /// treated as having no collision, a texture as a plain colour.
    MissingFile(u32),
    Generating,
    /// Downloading the render's files (textures): done / total.
    RenderFiles(usize, usize),
    Rendering,
    /// Baking the render failed; the pathing data is unaffected.
    RenderFailed,
}

/// Downloaded files by base file id.
type Files = HashMap<u32, Vec<u8>>;

/// Cached, lazily connecting source of pathing data.
///
/// Its fileserver connections come from a [`ConnectionPool`], which stores
/// made with [`PathingStore::share`] use too, so several threads can load
/// maps at once over a bounded number of connections.
pub struct PathingStore {
    dir: PathBuf,
    pool: Arc<ConnectionPool>,
    /// The asset manifest and its file id, once loaded.
    manifest: Option<Arc<(u32, AssetManifest)>>,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> PathingError + '_ {
    move |source| PathingError::Io { path: path.to_owned(), source }
}

impl PathingStore {
    /// A store with its own pool of [`MODEL_CONNECTIONS`] connections.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_pool(dir, Arc::new(ConnectionPool::fileserver(MODEL_CONNECTIONS)))
    }

    /// A store whose connections come from `pool`.
    pub fn with_pool(dir: impl Into<PathBuf>, pool: Arc<ConnectionPool>) -> Self {
        Self { dir: dir.into(), pool, manifest: None }
    }

    /// Another store on the same cache directory, sharing this one's
    /// connection pool and (if loaded) asset manifest; for another thread.
    pub fn share(&self) -> Self {
        Self { dir: self.dir.clone(), pool: self.pool.clone(), manifest: self.manifest.clone() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn pathing_path(&self, mapfile_id: u32, file_id: u32) -> PathBuf {
        self.dir.join("pathing").join(format!("{mapfile_id}-r{file_id}-v{FORMAT_VERSION}.path"))
    }

    fn render_path(&self, mapfile_id: u32, file_id: u32) -> PathBuf {
        self.dir.join("render").join(format!("{mapfile_id}-r{file_id}-v{RENDER_VERSION}.gwri"))
    }

    /// The cached entry for `mapfile_id` (any revision), newest first.
    pub fn cached_path(&self, mapfile_id: u32) -> Option<(u32, PathBuf)> {
        let prefix = format!("{mapfile_id}-r");
        let suffix = format!("-v{FORMAT_VERSION}.path");
        std::fs::read_dir(self.dir.join("pathing"))
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let file_id = name.strip_prefix(&prefix)?.strip_suffix(&suffix)?.parse().ok()?;
                let modified = e.metadata().and_then(|m| m.modified()).ok();
                Some((modified, file_id, e.path()))
            })
            .max_by_key(|(modified, _, _)| *modified)
            .map(|(_, file_id, path)| (file_id, path))
    }

    /// Whether the pathing data of revision `file_id` of a map is cached.
    pub fn has_pathing(&self, mapfile_id: u32, file_id: u32) -> bool {
        self.pathing_path(mapfile_id, file_id).is_file()
    }

    /// Whether the render of revision `file_id` of a map is cached.
    pub fn has_render(&self, mapfile_id: u32, file_id: u32) -> bool {
        self.render_path(mapfile_id, file_id).is_file()
    }

    /// Cached pathing data, without touching the network.
    pub fn cached(&self, mapfile_id: u32) -> Result<Option<PathingData>> {
        let Some((file_id, path)) = self.cached_path(mapfile_id) else { return Ok(None) };
        let data = std::fs::read(&path).map_err(io_err(&path))?;
        Ok(Some(PathingData::from_path_chunk(mapfile_id, file_id, &data)?))
    }

    /// Cached pathing data, or else download and generate it.
    pub fn load(&mut self, mapfile_id: u32, progress: impl FnMut(Progress)) -> Result<PathingData> {
        self.decode(mapfile_id, false, progress)
    }

    /// Download the current revision of the map and its models, generate
    /// the pathing data and cache it.
    pub fn fetch(&mut self, mapfile_id: u32, progress: impl FnMut(Progress)) -> Result<PathingData> {
        self.decode(mapfile_id, true, progress)
    }

    fn decode(&mut self, mapfile_id: u32, refresh: bool, progress: impl FnMut(Progress)) -> Result<PathingData> {
        let (file_id, chunk) = self.load_chunk(mapfile_id, refresh, progress)?;
        Ok(PathingData::from_path_chunk(mapfile_id, file_id, &chunk)?)
    }

    /// The stage-2 path chunk and the file id of its revision: from the
    /// cache unless `refresh`, else downloaded, generated and cached. A
    /// cache entry that does not decode is regenerated.
    pub fn load_chunk(
        &mut self,
        mapfile_id: u32,
        refresh: bool,
        mut progress: impl FnMut(Progress),
    ) -> Result<(u32, Vec<u8>)> {
        if !refresh && let Some((file_id, path)) = self.cached_path(mapfile_id) {
            let data = std::fs::read(&path).map_err(io_err(&path))?;
            if PathingData::from_path_chunk(mapfile_id, file_id, &data).is_ok() {
                return Ok((file_id, data));
            }
        }
        let chunk = self.generate(mapfile_id, &mut progress)?;
        let file_id = self.manifest(&mut progress)?.resolve(mapfile_id);
        write_atomic(&self.pathing_path(mapfile_id, file_id), &chunk)?;
        // The render is baked along with the pathing data, but a failure
        // there does not fail the pathing.
        if self.bake_render(mapfile_id, file_id, &mut progress).is_err() {
            progress(Progress::RenderFailed);
        }
        Ok((file_id, chunk))
    }

    /// The baked top-down render ([`render::WorldRender`] file) of the
    /// revision the cached pathing data was generated from, and that file
    /// id: from the cache, or else rendered and cached. Without cached
    /// pathing data, or with `refresh`, the current revision is used.
    pub fn load_render(
        &mut self,
        mapfile_id: u32,
        refresh: bool,
        mut progress: impl FnMut(Progress),
    ) -> Result<(u32, Vec<u8>)> {
        let file_id = match self.cached_path(mapfile_id).filter(|_| !refresh) {
            Some((file_id, _)) => file_id,
            None => self.manifest(&mut progress)?.resolve(mapfile_id),
        };
        let path = self.render_path(mapfile_id, file_id);
        if !refresh && let Ok(data) = std::fs::read(&path) {
            return Ok((file_id, data));
        }
        Ok((file_id, self.bake_render(mapfile_id, file_id, &mut progress)?))
    }

    /// Render revision `file_id` of a map and cache the image, even if it
    /// is cached already.
    pub fn bake_render(&mut self, mapfile_id: u32, file_id: u32, progress: &mut impl FnMut(Progress)) -> Result<Vec<u8>> {
        let (map, files) = self.render_inputs(file_id, progress)?;
        progress(Progress::Rendering);
        let data = render::bake(&map, &files)?.encode()?;
        write_atomic(&self.render_path(mapfile_id, file_id), &data)?;
        Ok(data)
    }

    /// Render the current revision of a map at `scale` world units per
    /// pixel, without caching it (for previews).
    pub fn render_preview(
        &mut self,
        mapfile_id: u32,
        scale: f32,
        mut progress: impl FnMut(Progress),
    ) -> Result<render::WorldRender> {
        let file_id = self.manifest(&mut progress)?.resolve(mapfile_id);
        let (map, files) = self.render_inputs(file_id, &mut progress)?;
        progress(Progress::Rendering);
        Ok(render::bake_with(&map, &files, scale, 32768)?)
    }

    /// A map revision and the files its render needs, by base id.
    fn render_inputs(
        &mut self,
        file_id: u32,
        progress: &mut impl FnMut(Progress),
    ) -> Result<(Vec<u8>, Files)> {
        let map = self.download(file_id, |done, total| progress(Progress::Map(done, total)))?;
        // The terrain textures and models, then the models' textures.
        let mut files = self.fetch_files(&render::required_files(&map)?, progress, Progress::RenderFiles, true)?;
        let textures = render::required_model_textures(&files);
        files.extend(self.fetch_files(&textures, progress, Progress::RenderFiles, true)?);
        Ok((map, files))
    }

    /// Files by base id (current revisions), from the cache or else the
    /// fileserver. Files the fileserver does not have are left out, and with
    /// `best_effort` so are files whose download keeps failing. `step`
    /// reports the count.
    fn fetch_files(
        &mut self,
        base_ids: &[u32],
        progress: &mut impl FnMut(Progress),
        step: fn(usize, usize) -> Progress,
        best_effort: bool,
    ) -> Result<Files> {
        let mut files = HashMap::new();
        let mut queue = Vec::new();
        for &base_id in base_ids {
            let file_id = self.manifest(progress)?.resolve(base_id);
            match std::fs::read(self.file_path(file_id)) {
                Ok(data) => {
                    files.insert(base_id, data);
                }
                Err(_) => queue.push((base_id, file_id)),
            }
        }
        progress(step(files.len(), base_ids.len()));
        if !queue.is_empty() {
            self.download_files(queue, &mut files, base_ids.len(), progress, step, best_effort)?;
        }
        Ok(files)
    }

    /// Download and generate the stage-2 path chunk, without caching it.
    pub fn generate(&mut self, mapfile_id: u32, progress: &mut impl FnMut(Progress)) -> Result<Vec<u8>> {
        let manifest_file = self.manifest(progress)?.resolve(mapfile_id);
        let map = self.download(manifest_file, |done, total| progress(Progress::Map(done, total)))?;
        let inputs = MapInputs::parse(&map)?;

        // Pathing must not be generated (and cached) without a model.
        let models = self.fetch_files(&inputs.required_models(), progress, Progress::Models, false)?;

        progress(Progress::Generating);
        let collision = inputs.collision(&models)?;
        // Zone obstacles are not generated yet.
        Ok(inputs.bloat_path(&collision, &[]))
    }

    /// The current asset manifest, from the cache or the fileserver.
    pub fn manifest(&mut self, progress: &mut impl FnMut(Progress)) -> Result<&AssetManifest> {
        if self.manifest.is_none() {
            let mut client = self.pool.get(|| progress(Progress::Connecting))?;
            let id = client.asset_manifest_id();
            let path = self.dir.join(format!("manifest-{id}.bin"));
            let data = match std::fs::read(&path) {
                Ok(data) => data,
                Err(_) => {
                    progress(Progress::Manifest);
                    let data = client.download(id, |_, _| {})?;
                    write_atomic(&path, &data)?;
                    data
                }
            };
            drop(client);
            self.manifest = Some(Arc::new((id, AssetManifest::parse(&data).map_err(FileConnError::from)?)));
        }
        Ok(&self.manifest.as_ref().expect("loaded above").1)
    }

    /// The asset manifest and its file id, if one was loaded; never
    /// connects.
    pub fn loaded_manifest(&self) -> Option<(u32, &AssetManifest)> {
        self.manifest.as_deref().map(|(id, m)| (*id, m))
    }

    fn file_path(&self, file_id: u32) -> PathBuf {
        self.dir.join("files").join(format!("{file_id}.bin"))
    }

    /// Download `(base_id, file_id)` files over up to
    /// [`MODEL_CONNECTIONS`] connections from the pool and cache them. Files missing from
    /// the fileserver are reported and skipped. A download that fails is
    /// retried on a new connection; if it keeps failing, it fails the whole
    /// call, or with `best_effort` is skipped like a missing file. `step`
    /// reports the count.
    fn download_files(
        &mut self,
        queue: Vec<(u32, u32)>,
        models: &mut Files,
        total: usize,
        progress: &mut impl FnMut(Progress),
        step: fn(usize, usize) -> Progress,
        best_effort: bool,
    ) -> Result<()> {
        let workers = MODEL_CONNECTIONS.min(queue.len());
        let queue = std::sync::Mutex::new(queue);
        let (tx, rx) = std::sync::mpsc::channel();
        let pool = &*self.pool;
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let (tx, queue) = (tx.clone(), &queue);
                scope.spawn(move || {
                    // Holds the connection until the queue is empty.
                    let mut client = match pool.get(|| {}) {
                        Ok(client) => client,
                        Err(e) => return tx.send(Err((e, 0))).unwrap_or(()),
                    };
                    loop {
                        let Some((base_id, file_id)) = queue.lock().expect("queue").pop() else { break };
                        let mut attempt = 0;
                        let result = loop {
                            match client.download(file_id, |_, _| {}) {
                                Err(FileConnError::NotFound(id)) => break Err((FileConnError::NotFound(id), file_id)),
                                Err(e) if attempt < RETRIES => {
                                    attempt += 1;
                                    if client.reconnect().is_err() {
                                        break Err((e, file_id));
                                    }
                                }
                                result => break result.map(|data| (base_id, file_id, data)).map_err(|e| (e, file_id)),
                            }
                        };
                        if tx.send(result).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut missing = 0;
            for result in rx {
                match result {
                    Ok((base_id, file_id, data)) => {
                        write_atomic(&self.file_path(file_id), &data)?;
                        models.insert(base_id, data);
                    }
                    Err((FileConnError::NotFound(id), _)) => {
                        missing += 1;
                        progress(Progress::MissingFile(id));
                    }
                    Err((_, file_id)) if best_effort && file_id != 0 => {
                        missing += 1;
                        progress(Progress::MissingFile(file_id));
                    }
                    Err((e, _)) => {
                        // Stop the other workers.
                        queue.lock().expect("queue").clear();
                        return Err(e.into());
                    }
                }
                progress(step(models.len() + missing, total));
            }
            Ok(())
        })
    }

    /// A map file by exact file id (as in [`PathingData::file_id`]), from
    /// the cache or the fileserver.
    pub fn map_file(&mut self, file_id: u32) -> Result<Vec<u8>> {
        self.download(file_id, |_, _| {})
    }

    /// A file by exact file id if it is in the cache; never connects.
    pub fn cached_file(&self, file_id: u32) -> Option<Vec<u8>> {
        std::fs::read(self.file_path(file_id)).ok()
    }

    /// A decompressed file by exact file id, from the cache or the
    /// fileserver.
    fn download(&mut self, file_id: u32, progress: impl FnMut(u32, u32)) -> Result<Vec<u8>> {
        let path = self.file_path(file_id);
        if let Ok(data) = std::fs::read(&path) {
            return Ok(data);
        }
        let data = self.pool.get(|| {})?.download(file_id, progress)?;
        write_atomic(&path, &data)?;
        Ok(data)
    }
}

/// Write via a temporary file so an interrupted write leaves no truncated
/// cache entry. The temporary name is unique, as threads sharing a cache
/// can write the same file (a model two maps use) at once.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
    std::fs::write(&tmp, data).map_err(io_err(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io_err(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathgen::testing::Fixture;

    /// Decoding the client's chunk gives the client's planes.
    #[test]
    fn decodes_client_chunk() {
        for f in Fixture::all() {
            let data = PathingData::from_path_chunk(f.pair.base_id, 0, &f.client_path).unwrap();
            assert_eq!(data.planes, f.client_planes);
            assert_eq!(data.plane_props, f.props.plane_props);
            assert!(!data.obstacles.is_empty());
        }
    }

    /// A cache entry is found without the network.
    #[test]
    fn cache_roundtrip() {
        let Some(f) = Fixture::all().next() else { return };
        let dir = tempfile::tempdir().unwrap();
        let store = PathingStore::new(dir.path());
        assert!(store.cached(f.pair.base_id).unwrap().is_none());
        write_atomic(&store.pathing_path(f.pair.base_id, 1234), &f.client_path).unwrap();
        let data = store.cached(f.pair.base_id).unwrap().unwrap();
        assert_eq!((data.mapfile_id, data.file_id), (f.pair.base_id, 1234));
        assert_eq!(data.planes, f.client_planes);
    }
}
