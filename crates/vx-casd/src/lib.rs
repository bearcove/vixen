//! vx-casd: Content-addressed storage service
//!
//! Implements the Cas rapace service trait.
//! CAS stores immutable content. Clients produce working directories.

use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use vx_cas_proto::*;

/// CAS service implementation
pub struct CasService {
    /// Root directory for CAS storage (typically .vx/cas)
    root: Utf8PathBuf,
    /// Next upload session ID
    next_upload_id: AtomicU64,
    /// In-progress chunked uploads
    uploads: Mutex<HashMap<u64, ChunkedUpload>>,
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
