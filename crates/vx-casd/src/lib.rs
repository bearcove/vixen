//! vx-casd: Content-addressed storage service
//!
//! Implements the Cas rapace service trait.
//! CAS stores immutable content. Clients produce working directories.

pub(crate) mod registry;
pub(crate) mod service;
pub(crate) mod tarball;
pub(crate) mod toolchain;
pub(crate) mod types;
pub(crate) mod utils;

use camino::Utf8PathBuf;

use std::sync::atomic::AtomicU64;

use crate::toolchain::ToolchainManager;
use crate::types::CasService;
use crate::utils::atomic_write;
pub use registry::RegistryManager;
use vx_cas_proto::{
    Blake3Hash, BlobHash, CacheKey, ManifestHash, ToolchainManifest, ToolchainSpecKey,
};

impl CasService {
    fn new(root: Utf8PathBuf) -> Self {
        Self {
            root,
            next_upload_id: AtomicU64::new(1),
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
