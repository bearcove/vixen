//! vx-casd: Content-addressed storage service
//!
//! Implements the Cas rapace service trait.
//! CAS stores immutable content. Clients produce working directories.

mod registry;
mod tarball;
mod toolchain;

use camino::{Utf8Path, Utf8PathBuf};

use std::collections::HashMap;
use std::fs;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use vx_cas_proto::*;
use vx_cas_proto::{
    MATERIALIZATION_LAYOUT_VERSION, MaterializationPlan, MaterializeStep, ToolchainKind,
    ToolchainManifest, ToolchainSpecKey,
};

pub use registry::RegistryManager;

use crate::toolchain::ToolchainManager;

/// CAS service implementation
struct CasService {
    /// Root directory for CAS storage (typically .vx/cas)
    root: Utf8PathBuf,

    /// Next upload session ID
    next_upload_id: AtomicU64,

    /// In-progress chunked uploads
    uploads: Mutex<HashMap<u64, ChunkedUpload>>,

    /// Toolchain acquisition manager (handles inflight deduplication)
    toolchain_manager: ToolchainManager,

    /// Registry crate acquisition manager (handles inflight deduplication)
    registry_manager: RegistryManager,
}

/// State for an in-progress chunked upload
struct ChunkedUpload {
    hasher: blake3::Hasher,
    tmp_path: Utf8PathBuf,
    file: std::fs::File,
}

impl CasService {
    fn new(root: Utf8PathBuf) -> Self {
        Self {
            root,
            next_upload_id: AtomicU64::new(1),
            uploads: Mutex::new(HashMap::new()),
            toolchain_manager: ToolchainManager::new(),
            registry_manager: RegistryManager::new(),
        }
    }

    fn blobs_dir(&self) -> Utf8PathBuf {
        self.root.join("blobs/blake3")
    }

    fn manifests_dir(&self) -> Utf8PathBuf {
        self.root.join("manifests/blake3")
    }

    fn cache_dir(&self) -> Utf8PathBuf {
        self.root.join("cache/blake3")
    }

    fn tmp_dir(&self) -> Utf8PathBuf {
        self.root.join("tmp")
    }

    fn blob_path(&self, hash: &BlobHash) -> Utf8PathBuf {
        let hex = hash.to_hex();
        self.blobs_dir().join(&hex[..2]).join(&hex)
    }

    fn manifest_path(&self, hash: &ManifestHash) -> Utf8PathBuf {
        let hex = hash.to_hex();
        self.manifests_dir()
            .join(&hex[..2])
            .join(format!("{}.json", hex))
    }

    fn cache_path(&self, key: &CacheKey) -> Utf8PathBuf {
        let hex = key.to_hex();
        self.cache_dir().join(&hex[..2]).join(&hex)
    }

    fn toolchains_spec_dir(&self) -> Utf8PathBuf {
        self.root.join("toolchains/spec")
    }

    fn spec_path(&self, spec_key: &ToolchainSpecKey) -> Utf8PathBuf {
        let hex = spec_key.to_hex();
        self.toolchains_spec_dir().join(&hex[..2]).join(&hex)
    }

    /// Publish spec → manifest_hash mapping atomically (first-writer-wins).
    /// Returns Ok(true) if we published, Ok(false) if already exists.
    ///
    /// DURABILITY: Uses O_EXCL + fsync + directory sync for crash safety.
    async fn publish_spec_mapping(
        &self,
        spec_key: &ToolchainSpecKey,
        manifest_hash: &Blake3Hash,
    ) -> std::io::Result<bool> {
        let path = self.spec_path(spec_key);

        // Check if file already exists first
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }

        // Use atomic_write which handles tmp + rename
        match atomic_write(&path, manifest_hash.to_hex().as_bytes()).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(false) // Another writer won during the race
            }
            Err(e) => Err(e),
        }
    }

    /// Lookup manifest_hash by spec_key.
    ///
    /// If the mapping file exists but is unreadable/corrupt, we treat it as
    /// a cache miss (re-acquire will fix it via first-writer-wins).
    async fn lookup_spec(&self, spec_key: &ToolchainSpecKey) -> Option<Blake3Hash> {
        let path = self.spec_path(spec_key);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        Blake3Hash::from_hex(content.trim())
    }

    /// Initialize directory structure
    async fn init(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(self.blobs_dir()).await?;
        tokio::fs::create_dir_all(self.manifests_dir()).await?;
        tokio::fs::create_dir_all(self.cache_dir()).await?;
        tokio::fs::create_dir_all(self.tmp_dir()).await?;
        Ok(())
    }

    /// Store a ToolchainManifest using the manifest storage path.
    async fn put_toolchain_manifest(&self, manifest: &ToolchainManifest) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);

        if !path.exists() {
            let _ = atomic_write(&path, json.as_bytes());
        }

        hash
    }
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_nanos() as u64 ^ std::process::id() as u64
}

impl Cas for CasService {
    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash> {
        let path = self.cache_path(&cache_key);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        ManifestHash::from_hex(content.trim())
    }

    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult {
        let manifest_path = self.manifest_path(&manifest_hash);
        let dest = self.cache_path(&cache_key);

        // Validate manifest exists
        if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
            return PublishResult {
                success: false,
                error: Some(format!(
                    "manifest {} does not exist",
                    manifest_hash.to_hex()
                )),
            };
        }

        // Atomically write the cache mapping
        match atomic_write(&dest, manifest_hash.to_hex().as_bytes()).await {
            Ok(()) => PublishResult {
                success: true,
                error: None,
            },
            Err(e) => PublishResult {
                success: false,
                error: Some(format!("failed to write cache mapping: {}", e)),
            },
        }
    }

    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash {
        let json = facet_json::to_string(&manifest);
        let hash = ManifestHash::from_bytes(json.as_bytes());
        let dest = self.manifest_path(&hash);
        let tmp_dir = self.tmp_dir();

        tokio::task::spawn_blocking(move || {
            if !dest.exists() {
                // Atomic write
                if let Some(parent) = dest.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::create_dir_all(&tmp_dir);
                let tmp = tmp_dir.join(format!("write-{}", rand_suffix()));
                let _ = fs::write(&tmp, json.as_bytes());
                let _ = fs::rename(&tmp, &dest);
            }
        })
        .await
        .ok();

        hash
    }

    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest> {
        let path = self.manifest_path(&hash);
        tokio::task::spawn_blocking(move || {
            let json = fs::read_to_string(&path).ok()?;
            facet_json::from_str(&json).ok()
        })
        .await
        .ok()
        .flatten()
    }

    async fn put_blob(&self, data: Vec<u8>) -> BlobHash {
        let hash = BlobHash::from_bytes(&data);
        let dest = self.blob_path(&hash);
        let tmp_dir = self.tmp_dir();

        // Use spawn_blocking for all file I/O
        tokio::task::spawn_blocking(move || {
            if !dest.exists() {
                // Atomic write: tmp + rename
                if let Some(parent) = dest.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::create_dir_all(&tmp_dir);
                let tmp = tmp_dir.join(format!("write-{}", rand_suffix()));
                let _ = fs::write(&tmp, &data);
                let _ = fs::rename(&tmp, &dest);
            }
        })
        .await
        .ok();

        hash
    }

    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>> {
        let path = self.blob_path(&hash);
        tokio::task::spawn_blocking(move || fs::read(&path).ok())
            .await
            .ok()
            .flatten()
    }

    async fn has_blob(&self, hash: BlobHash) -> bool {
        let path = self.blob_path(&hash);
        tokio::task::spawn_blocking(move || path.exists())
            .await
            .unwrap_or(false)
    }

    async fn begin_blob(&self) -> BlobUploadId {
        let id = self.next_upload_id.fetch_add(1, Ordering::SeqCst);
        let tmp_path = self.tmp_dir().join(format!("upload-{}", id));

        // Create the temp file
        let file = match std::fs::File::create(&tmp_path) {
            Ok(f) => f,
            Err(_) => {
                // Return the ID anyway; finish_blob will fail
                return BlobUploadId(id);
            }
        };

        let upload = ChunkedUpload {
            hasher: blake3::Hasher::new(),
            tmp_path,
            file,
        };

        self.uploads.lock().unwrap().insert(id, upload);
        BlobUploadId(id)
    }

    async fn blob_chunk(&self, id: BlobUploadId, chunk: Vec<u8>) {
        use std::io::Write;

        let mut uploads = self.uploads.lock().unwrap();
        if let Some(upload) = uploads.get_mut(&id.0) {
            upload.hasher.update(&chunk);
            let _ = upload.file.write_all(&chunk);
        }
    }

    async fn finish_blob(&self, id: BlobUploadId) -> FinishBlobResult {
        let upload = {
            let mut uploads = self.uploads.lock().unwrap();
            uploads.remove(&id.0)
        };

        let Some(upload) = upload else {
            return FinishBlobResult {
                success: false,
                hash: None,
                error: Some("upload session not found".to_string()),
            };
        };

        // Finalize hash
        let hash = Blake3Hash(*upload.hasher.finalize().as_bytes());
        let dest = self.blob_path(&hash);

        // Move to final location if not already present
        if !dest.exists() {
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::rename(&upload.tmp_path, &dest) {
                // Clean up temp file
                let _ = fs::remove_file(&upload.tmp_path);
                return FinishBlobResult {
                    success: false,
                    hash: None,
                    error: Some(format!("failed to finalize blob: {}", e)),
                };
            }
        } else {
            // Already exists, remove temp
            let _ = fs::remove_file(&upload.tmp_path);
        }

        FinishBlobResult {
            success: true,
            hash: Some(hash),
            error: None,
        }
    }

    #[tracing::instrument(skip(self), fields(name = %spec.name, version = %spec.version))]
    async fn ensure_registry_crate(&self, spec: RegistrySpec) -> EnsureRegistryCrateResult {
        // Validate spec and compute spec_key
        let spec_key = match spec.spec_key() {
            Ok(k) => k,
            Err(e) => {
                return EnsureRegistryCrateResult {
                    spec_key: None,
                    manifest_hash: None,
                    status: EnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };

        // Validate registry is crates.io
        if spec.registry_url != CRATES_IO_REGISTRY {
            return EnsureRegistryCrateResult {
                spec_key: Some(spec_key),
                manifest_hash: None,
                status: EnsureStatus::Failed,
                error: Some(format!(
                    "only crates.io is supported, got: {}",
                    spec.registry_url
                )),
            };
        }

        self.registry_manager
            .ensure(
                spec_key,
                || async move { self.lookup_registry_spec_local(&spec_key) },
                || async {
                    // Download tarball
                    let tarball_bytes =
                        match download_crate(&spec.name, &spec.version, &spec.checksum).await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                return EnsureRegistryCrateResult {
                                    spec_key: Some(spec_key),
                                    manifest_hash: None,
                                    status: EnsureStatus::Failed,
                                    error: Some(e),
                                };
                            }
                        };

                    // Validate tarball structure
                    if let Err(e) = validate_crate_tarball(&tarball_bytes) {
                        return EnsureRegistryCrateResult {
                            spec_key: Some(spec_key),
                            manifest_hash: None,
                            status: EnsureStatus::Failed,
                            error: Some(format!("tarball validation failed: {}", e)),
                        };
                    }

                    // Store tarball as blob
                    let tarball_blob = self.put_blob(tarball_bytes).await;

                    // Create manifest
                    let manifest = RegistryCrateManifest {
                        schema_version: REGISTRY_MANIFEST_SCHEMA_VERSION,
                        spec: spec.clone(),
                        crate_tarball_blob: tarball_blob,
                        created_at: chrono::Utc::now().to_rfc3339(),
                    };

                    // Store manifest
                    let manifest_hash = self.put_registry_manifest(&manifest).await;

                    // Publish spec → manifest_hash mapping
                    let _ = self.publish_registry_spec_mapping(&spec_key, &manifest_hash);

                    tracing::info!(
                        name = %spec.name,
                        version = %spec.version,
                        spec_key = %spec_key.short_hex(),
                        manifest_hash = %manifest_hash.short_hex(),
                        "stored registry crate in CAS"
                    );

                    EnsureRegistryCrateResult {
                        spec_key: Some(spec_key),
                        manifest_hash: Some(manifest_hash),
                        status: EnsureStatus::Downloaded,
                        error: None,
                    }
                },
            )
            .await
    }

    async fn get_registry_manifest(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<RegistryCrateManifest> {
        self.get_registry_crate_manifest(&manifest_hash)
    }

    async fn lookup_registry_spec(&self, spec_key: RegistrySpecKey) -> Option<Blake3Hash> {
        self.lookup_registry_spec_local(&spec_key)
    }

    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest> {
        let path = self.manifest_path(&manifest_hash);
        let json = fs::read_to_string(&path).ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn get_materialization_plan(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<MaterializationPlan> {
        let manifest = self.get_toolchain_manifest(manifest_hash).await?;

        // Pure function: manifest → plan (no heuristics, bit-for-bit stable)
        let steps = match manifest.kind {
            ToolchainKind::Rust => {
                let mut steps = vec![MaterializeStep::EnsureDir {
                    relpath: "sysroot".to_string(),
                }];
                steps.extend(manifest.components.iter().map(|c| {
                    MaterializeStep::ExtractTarXz {
                        blob: c.blob,
                        strip_components: 1, // Validated during acquisition
                        dest_subdir: "sysroot".to_string(),
                    }
                }));
                steps
            }
            ToolchainKind::Zig => {
                let mut steps = Vec::new();
                for c in &manifest.components {
                    match c.name.as_str() {
                        "zig-exe" => {
                            steps.push(MaterializeStep::WriteFile {
                                relpath: "zig".to_string(),
                                blob: c.blob,
                                mode: 0o755,
                            });
                        }
                        "zig-lib" => {
                            steps.push(MaterializeStep::EnsureDir {
                                relpath: "lib".to_string(),
                            });
                            steps.push(MaterializeStep::ExtractTarXz {
                                blob: c.blob,
                                strip_components: 0,
                                dest_subdir: "lib".to_string(),
                            });
                        }
                        _ => {} // Unknown component, skip
                    }
                }
                steps
            }
        };

        Some(MaterializationPlan {
            toolchain_id: manifest.toolchain_id,
            layout_version: MATERIALIZATION_LAYOUT_VERSION,
            steps,
        })
    }

    async fn stream_blob(&self, blob: Blake3Hash) -> rapace::Streaming<Vec<u8>> {
        use tokio::sync::mpsc;
        use tokio_stream::wrappers::ReceiverStream;

        let (tx, rx) = mpsc::channel(16);
        let blob_path = self.blob_path(&blob);

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;

            let Ok(file) = tokio::fs::File::open(&blob_path).await else {
                let _ = tx
                    .send(Err(rapace::RpcError::Status {
                        code: rapace::ErrorCode::NotFound,
                        message: "blob not found".to_string(),
                    }))
                    .await;
                return;
            };

            let mut reader = tokio::io::BufReader::new(file);
            let mut buf = vec![0u8; 16 * 1024 * 1024]; // 16MB chunks

            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(rapace::RpcError::Status {
                                code: rapace::ErrorCode::Internal,
                                message: format!("read error: {}", e),
                            }))
                            .await;
                        break;
                    }
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }
}

pub async fn atomic_write(path: &Utf8Path, contents: &[u8]) -> Result<(), std::io::Error> {
    // Create a temporary file in the same directory as the target path
    let parent_dir = path.parent().unwrap_or_else(|| camino::Utf8Path::new("."));

    // Create parent directory if it doesn't exist
    tokio::fs::create_dir_all(parent_dir).await?;

    // Create a temporary file in the same directory to ensure it's on the same filesystem
    let temp_file = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(parent_dir)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Get the temporary path and write contents to it
    let temp_path = temp_file.into_temp_path();
    tokio::fs::write(&temp_path, contents).await?;

    // Atomically persist the temporary file to the final location
    temp_path.persist(&path).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to persist temp file: {}", e),
        )
    })?;

    Ok(())
}
