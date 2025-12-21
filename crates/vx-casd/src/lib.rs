//! vx-casd: Content-addressed storage service
//!
//! Implements the Cas rapace service trait.
//! CAS stores immutable content. Clients produce working directories.

use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vx_cas_proto::*;
use vx_toolchain_proto::*;

/// CAS service implementation
pub struct CasService {
    /// Root directory for CAS storage (typically .vx/cas)
    root: Utf8PathBuf,
    /// Next upload session ID
    next_upload_id: AtomicU64,
    /// In-progress chunked uploads
    uploads: Mutex<HashMap<u64, ChunkedUpload>>,
    /// Toolchain acquisition manager (handles inflight deduplication)
    toolchain_manager: ToolchainManager,
}

/// State for an in-progress chunked upload
struct ChunkedUpload {
    hasher: blake3::Hasher,
    tmp_path: Utf8PathBuf,
    file: std::fs::File,
}

impl CasService {
    pub fn new(root: Utf8PathBuf) -> Self {
        Self {
            root,
            next_upload_id: AtomicU64::new(1),
            uploads: Mutex::new(HashMap::new()),
            toolchain_manager: ToolchainManager::new(),
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

    fn spec_path(&self, spec_key: &SpecKey) -> Utf8PathBuf {
        let hex = spec_key.to_hex();
        self.toolchains_spec_dir().join(&hex[..2]).join(&hex)
    }

    /// Publish spec → manifest_hash mapping atomically (first-writer-wins).
    /// Returns Ok(true) if we published, Ok(false) if already exists.
    ///
    /// DURABILITY: Uses O_EXCL + fsync + directory sync for crash safety.
    fn publish_spec_mapping(
        &self,
        spec_key: &SpecKey,
        manifest_hash: &Blake3Hash,
    ) -> std::io::Result<bool> {
        use std::fs::{File, OpenOptions};
        use std::io::Write;

        let path = self.spec_path(spec_key);

        let parent = path.parent().expect("spec path has parent");
        fs::create_dir_all(parent)?;

        // Use O_EXCL (create_new) for atomic first-writer-wins
        match OpenOptions::new()
            .write(true)
            .create_new(true) // O_EXCL - fails if exists
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(manifest_hash.to_hex().as_bytes())?;
                // Ensure data is on disk
                file.sync_all()?;

                // Sync parent directory to ensure the directory entry is durable
                // (required on some filesystems for crash safety)
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }

                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(false) // Another writer won
            }
            Err(e) => Err(e),
        }
    }

    /// Lookup manifest_hash by spec_key.
    ///
    /// If the mapping file exists but is unreadable/corrupt, we treat it as
    /// a cache miss (re-acquire will fix it via first-writer-wins).
    fn lookup_spec(&self, spec_key: &SpecKey) -> Option<Blake3Hash> {
        let path = self.spec_path(spec_key);
        let content = std::fs::read_to_string(&path).ok()?;
        Blake3Hash::from_hex(content.trim())
    }

    /// Initialize directory structure
    pub fn init(&self) -> std::io::Result<()> {
        fs::create_dir_all(self.blobs_dir())?;
        fs::create_dir_all(self.manifests_dir())?;
        fs::create_dir_all(self.cache_dir())?;
        fs::create_dir_all(self.tmp_dir())?;
        Ok(())
    }

    /// Atomically write data to a file via tmp + rename
    fn atomic_write(&self, dest: &Utf8PathBuf, data: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.tmp_dir().join(format!("write-{}", rand_suffix()));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, dest)?;
        Ok(())
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
        let content = fs::read_to_string(&path).ok()?;
        ManifestHash::from_hex(content.trim())
    }

    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult {
        // Validate manifest exists
        let manifest_path = self.manifest_path(&manifest_hash);
        if !manifest_path.exists() {
            return PublishResult {
                success: false,
                error: Some(format!(
                    "manifest {} does not exist",
                    manifest_hash.to_hex()
                )),
            };
        }

        // Optionally: validate all blobs referenced by manifest exist
        // For v0, we trust that blobs were written before manifest

        // Atomically write the cache mapping
        let dest = self.cache_path(&cache_key);
        match self.atomic_write(&dest, manifest_hash.to_hex().as_bytes()) {
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

        if !dest.exists() {
            let _ = self.atomic_write(&dest, json.as_bytes());
        }

        hash
    }

    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest> {
        let path = self.manifest_path(&hash);
        let json = fs::read_to_string(&path).ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn put_blob(&self, data: Vec<u8>) -> BlobHash {
        let hash = BlobHash::from_bytes(&data);
        let dest = self.blob_path(&hash);

        if !dest.exists() {
            let _ = self.atomic_write(&dest, &data);
        }

        hash
    }

    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>> {
        let path = self.blob_path(&hash);
        fs::read(&path).ok()
    }

    async fn has_blob(&self, hash: BlobHash) -> bool {
        self.blob_path(&hash).exists()
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
}

// =============================================================================
// Toolchain Manager (Inflight Deduplication)
// =============================================================================

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureToolchainResult>>;

/// Manages in-flight toolchain acquisitions with deduplication.
///
/// Inflight entries are never removed. This is intentional:
/// - Memory cost is negligible (one OnceCell per unique SpecKey)
/// - Avoids race between CAS lookup miss and inflight insert
/// - Once CAS has the mapping, lookup_fn fast-paths and OnceCell is never awaited
pub struct ToolchainManager {
    inflight: tokio::sync::Mutex<HashMap<SpecKey, InflightFuture>>,
}

impl ToolchainManager {
    pub fn new() -> Self {
        Self {
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ensure a toolchain, deduplicating concurrent requests.
    ///
    /// `lookup_fn` is async because CAS is remote in production.
    pub async fn ensure<L, LFut, A, AFut>(
        &self,
        spec_key: SpecKey,
        lookup_fn: L,
        acquire_fn: A,
    ) -> EnsureToolchainResult
    where
        L: Fn() -> LFut,
        LFut: std::future::Future<Output = Option<Blake3Hash>>,
        A: FnOnce() -> AFut,
        AFut: std::future::Future<Output = EnsureToolchainResult>,
    {
        // Fast path: check if already in CAS (async RPC)
        if let Some(manifest_hash) = lookup_fn().await {
            return EnsureToolchainResult {
                spec_key: Some(spec_key),
                toolchain_id: None, // Caller should read manifest for ID
                manifest_hash: Some(manifest_hash),
                status: EnsureStatus::Hit,
                error: None,
            };
        }

        // Get or create inflight entry
        let cell = {
            let mut inflight = self.inflight.lock().await;
            inflight
                .entry(spec_key)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        // Initialize if we're first, otherwise wait
        cell.get_or_init(|| acquire_fn()).await.clone()

        // NOTE: We intentionally do NOT remove entries from inflight.
        // See struct doc comment for rationale.
    }
}

// =============================================================================
// CasToolchain Implementation
// =============================================================================

impl CasToolchain for CasService {
    #[tracing::instrument(skip(self), fields(spec_key = tracing::field::Empty))]
    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Validate and compute spec_key first
        let spec_key = match spec.spec_key() {
            Ok(k) => {
                tracing::Span::current().record("spec_key", k.short_hex());
                k
            }
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: None, // Can't compute - no sentinel hash
                    toolchain_id: None,
                    manifest_hash: None,
                    status: EnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };

        self.toolchain_manager
            .ensure(
                spec_key,
                // Async lookup (CAS is remote in production)
                || async move { self.lookup_spec(&spec_key) },
                || async {
                    // Convert proto spec to vx_toolchain types
                    let channel = match &spec.channel {
                        RustChannel::Stable => vx_toolchain::Channel::Stable,
                        RustChannel::Beta => vx_toolchain::Channel::Beta,
                        RustChannel::Nightly { date } => {
                            vx_toolchain::Channel::Nightly { date: date.clone() }
                        }
                    };

                    let toolchain_spec = vx_toolchain::RustToolchainSpec {
                        channel,
                        host: spec.host.clone(),
                        target: if spec.target == spec.host {
                            None
                        } else {
                            Some(spec.target.clone())
                        },
                    };

                    // Acquire (downloads, verifies, returns tarballs)
                    let acquired = match vx_toolchain::acquire_rust_toolchain(&toolchain_spec).await
                    {
                        Ok(a) => a,
                        Err(e) => {
                            return EnsureToolchainResult {
                                spec_key: Some(spec_key),
                                toolchain_id: None,
                                manifest_hash: None,
                                status: EnsureStatus::Failed,
                                error: Some(format!("{}", e)),
                            };
                        }
                    };

                    // Validate tarball structure (PERF: double decompression, see note)
                    if let Err(e) = validate_tarball_structure(&acquired.rustc_tarball) {
                        return EnsureToolchainResult {
                            spec_key: Some(spec_key),
                            toolchain_id: None,
                            manifest_hash: None,
                            status: EnsureStatus::Failed,
                            error: Some(format!("rustc tarball invalid: {}", e)),
                        };
                    }
                    if let Err(e) = validate_tarball_structure(&acquired.rust_std_tarball) {
                        return EnsureToolchainResult {
                            spec_key: Some(spec_key),
                            toolchain_id: None,
                            manifest_hash: None,
                            status: EnsureStatus::Failed,
                            error: Some(format!("rust-std tarball invalid: {}", e)),
                        };
                    }

                    // Store component blobs
                    let rustc_blob = self.put_blob(acquired.rustc_tarball.clone()).await;
                    let rust_std_blob = self.put_blob(acquired.rust_std_tarball.clone()).await;

                    // Build manifest
                    let manifest = ToolchainManifest {
                        schema_version: TOOLCHAIN_MANIFEST_SCHEMA_VERSION,
                        kind: ToolchainKind::Rust,
                        spec_key,
                        toolchain_id: acquired.id.0,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        rust_manifest_date: Some(acquired.manifest_date.clone()),
                        rust_version: Some(acquired.rustc_version.clone()),
                        zig_version: None,
                        components: vec![
                            ToolchainComponentBlob {
                                name: "rustc".to_string(),
                                target: Some(spec.host.clone()),
                                compression: "xz".to_string(),
                                blob: rustc_blob,
                                sha256: acquired.rustc_manifest_sha256.clone(),
                                size_bytes: acquired.rustc_tarball.len() as u64,
                            },
                            ToolchainComponentBlob {
                                name: "rust-std".to_string(),
                                target: Some(spec.target.clone()),
                                compression: "xz".to_string(),
                                blob: rust_std_blob,
                                sha256: acquired.rust_std_manifest_sha256.clone(),
                                size_bytes: acquired.rust_std_tarball.len() as u64,
                            },
                        ],
                    };

                    // Store manifest using put_toolchain_manifest (not put_blob!)
                    let manifest_hash = self.put_toolchain_manifest(&manifest).await;

                    // Publish spec → manifest_hash mapping (atomic, first-writer-wins)
                    let _ = self.publish_spec_mapping(&spec_key, &manifest_hash);

                    tracing::info!(
                        spec_key = %spec_key.short_hex(),
                        toolchain_id = %acquired.id.short_hex(),
                        manifest_hash = %manifest_hash.short_hex(),
                        "stored Rust toolchain in CAS"
                    );

                    EnsureToolchainResult {
                        spec_key: Some(spec_key),
                        toolchain_id: Some(acquired.id.0),
                        manifest_hash: Some(manifest_hash),
                        status: EnsureStatus::Downloaded,
                        error: None,
                    }
                },
            )
            .await
    }

    async fn ensure_zig_toolchain(&self, _spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // TODO: Implement Zig toolchain acquisition
        EnsureToolchainResult {
            spec_key: None,
            toolchain_id: None,
            manifest_hash: None,
            status: EnsureStatus::Failed,
            error: Some("Zig toolchain acquisition not yet implemented".to_string()),
        }
    }

    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest> {
        let path = self.manifest_path(&manifest_hash);
        let json = fs::read_to_string(&path).ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn lookup_toolchain_spec(&self, spec_key: SpecKey) -> Option<Blake3Hash> {
        self.lookup_spec(&spec_key)
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

/// Store a ToolchainManifest using the manifest storage path.
impl CasService {
    async fn put_toolchain_manifest(&self, manifest: &ToolchainManifest) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);

        if !path.exists() {
            let _ = self.atomic_write(&path, json.as_bytes());
        }

        hash
    }
}

// =============================================================================
// Tarball Validation
// =============================================================================

/// Validate that a tarball has exactly one top-level directory.
/// Returns the number of components to strip (always 1 for valid tarballs).
///
/// PERF: This decompresses the entire tarball to validate structure.
/// Future optimization: validate during extraction in execd.
fn validate_tarball_structure(tarball_bytes: &[u8]) -> Result<u32, String> {
    use std::io::Read;
    use xz2::read::XzDecoder;

    let mut decoder = XzDecoder::new(tarball_bytes);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("xz decompression failed: {}", e))?;

    let mut archive = tar::Archive::new(decompressed.as_slice());
    let mut top_level_dirs = std::collections::HashSet::new();

    for entry in archive.entries().map_err(|e| format!("tar error: {}", e))? {
        let entry = entry.map_err(|e| format!("tar entry error: {}", e))?;
        let path = entry.path().map_err(|e| format!("path error: {}", e))?;

        // Get the first component
        if let Some(first) = path.components().next() {
            if let std::path::Component::Normal(name) = first {
                top_level_dirs.insert(name.to_string_lossy().to_string());
            }
        }
    }

    if top_level_dirs.len() != 1 {
        return Err(format!(
            "tarball must have exactly one top-level directory, found {}: {:?}",
            top_level_dirs.len(),
            top_level_dirs
        ));
    }

    Ok(1) // strip_components = 1
}
