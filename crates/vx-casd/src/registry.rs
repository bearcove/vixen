//! Registry crate acquisition for casd.
//!
//! Downloads .crate tarballs from crates.io, validates checksums and structure,
//! and stores them in CAS.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use vx_cas_proto::{Blake3Hash, Cas};
use vx_cas_proto::{
    CRATES_IO_REGISTRY, EnsureRegistryCrateResult, EnsureStatus, REGISTRY_MANIFEST_SCHEMA_VERSION,
    RegistryCrateManifest, RegistrySpec, RegistrySpecKey,
};

use crate::CasService;

/// Maximum retry attempts for transient failures
const MAX_RETRIES: u32 = 3;

/// Initial backoff duration
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Maximum backoff duration
const MAX_BACKOFF: Duration = Duration::from_secs(10);

// =============================================================================
// Registry Manager (Inflight Deduplication)
// =============================================================================

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureRegistryCrateResult>>;

/// Manages in-flight registry crate acquisitions with deduplication.
///
/// Same design as ToolchainManager: inflight entries are never removed.
pub struct RegistryManager {
    inflight: tokio::sync::Mutex<HashMap<RegistrySpecKey, InflightFuture>>,
}

impl RegistryManager {
    pub fn new() -> Self {
        Self {
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ensure a registry crate, deduplicating concurrent requests.
    pub async fn ensure<L, LFut, A, AFut>(
        &self,
        spec_key: RegistrySpecKey,
        lookup_fn: L,
        acquire_fn: A,
    ) -> EnsureRegistryCrateResult
    where
        L: Fn() -> LFut,
        LFut: std::future::Future<Output = Option<Blake3Hash>>,
        A: FnOnce() -> AFut,
        AFut: std::future::Future<Output = EnsureRegistryCrateResult>,
    {
        // Fast path: check if already in CAS
        if let Some(manifest_hash) = lookup_fn().await {
            return EnsureRegistryCrateResult {
                spec_key: Some(spec_key),
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
    }
}

// =============================================================================
// Download and Validation
// =============================================================================

/// Download a crate tarball from crates.io with retries.
async fn download_crate(
    name: &str,
    version: &str,
    expected_checksum: &str,
) -> Result<Vec<u8>, String> {
    let url = format!(
        "https://crates.io/api/v1/crates/{}/{}/download",
        name, version
    );

    let mut last_error = String::new();
    let mut backoff = INITIAL_BACKOFF;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            tracing::info!(
                attempt = attempt + 1,
                backoff_ms = backoff.as_millis(),
                "retrying crate download"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }

        match download_crate_attempt(&url, expected_checksum).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                last_error = e.clone();
                // Check if error is retryable
                if !is_retryable_error(&e) {
                    return Err(e);
                }
                tracing::warn!(error = %e, "transient download error");
            }
        }
    }

    Err(format!(
        "download failed after {} attempts: {}",
        MAX_RETRIES, last_error
    ))
}

/// Single download attempt
async fn download_crate_attempt(url: &str, expected_checksum: &str) -> Result<Vec<u8>, String> {
    tracing::info!(url = %url, "downloading crate");

    let response = reqwest::get(url)
        .await
        .map_err(|e| format!("request failed: {}", e))?;

    let status = response.status();

    // Check for rate limiting
    if status.as_u16() == 429 {
        return Err("rate limited (429)".to_string());
    }

    // Check for server errors (retryable)
    if status.is_server_error() {
        return Err(format!("server error: {}", status));
    }

    // Check for client errors (not retryable except 429)
    if !status.is_success() {
        return Err(format!("HTTP error: {}", status));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read response: {}", e))?;

    // Verify SHA256 checksum
    let actual_checksum = compute_sha256(&bytes);
    if actual_checksum.to_lowercase() != expected_checksum.to_lowercase() {
        return Err(format!(
            "checksum mismatch: expected {}, got {}",
            expected_checksum, actual_checksum
        ));
    }

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        checksum = %actual_checksum,
        "crate downloaded and verified"
    );

    Ok(bytes.to_vec())
}

fn is_retryable_error(error: &str) -> bool {
    error.contains("rate limited")
        || error.contains("server error")
        || error.contains("request failed")
}

fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Validate tarball structure:
/// - Exactly one top-level directory
/// - No absolute paths
/// - No .. components
/// - No symlink escapes
fn validate_crate_tarball(tarball_bytes: &[u8]) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    // .crate files are gzipped tarballs
    let mut decoder = GzDecoder::new(tarball_bytes);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("gzip decompression failed: {}", e))?;

    let mut archive = tar::Archive::new(decompressed.as_slice());
    let mut top_level_dirs = std::collections::HashSet::new();

    for entry in archive.entries().map_err(|e| format!("tar error: {}", e))? {
        let entry = entry.map_err(|e| format!("tar entry error: {}", e))?;
        let path = entry.path().map_err(|e| format!("path error: {}", e))?;

        // Check for absolute paths
        if path.is_absolute() {
            return Err(format!("absolute path in tarball: {:?}", path));
        }

        // Check for .. components
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(format!("parent dir (..) in tarball path: {:?}", path));
            }
        }

        // Reject all symlinks - we don't preserve them through the CAS→execd pipeline,
        // and extracting them as regular files would cause silent breakage.
        // Cargo's packaging typically strips symlinks anyway.
        if entry.header().entry_type().is_symlink() {
            return Err(format!(
                "symlinks not supported in crate tarballs: {:?}",
                path
            ));
        }

        // Track top-level directory
        if let Some(first) = path.components().next() {
            if let std::path::Component::Normal(name) = first {
                top_level_dirs.insert(name.to_string_lossy().to_string());
            }
        }
    }

    // Exactly one top-level directory
    if top_level_dirs.len() != 1 {
        return Err(format!(
            "tarball must have exactly one top-level directory, found {}: {:?}",
            top_level_dirs.len(),
            top_level_dirs
        ));
    }

    Ok(())
}

// =============================================================================
// CasRegistry Implementation
// =============================================================================

impl CasService {
    /// Get the registry spec directory
    fn registry_spec_dir(&self) -> camino::Utf8PathBuf {
        self.root.join("registry/spec")
    }

    /// Get the path for a registry spec mapping
    fn registry_spec_path(&self, spec_key: &RegistrySpecKey) -> camino::Utf8PathBuf {
        let hex = spec_key.to_hex();
        self.registry_spec_dir().join(&hex[..2]).join(&hex)
    }

    /// Publish registry spec → manifest_hash mapping atomically (first-writer-wins).
    fn publish_registry_spec_mapping(
        &self,
        spec_key: &RegistrySpecKey,
        manifest_hash: &Blake3Hash,
    ) -> std::io::Result<bool> {
        use std::fs::{File, OpenOptions};
        use std::io::Write;

        let path = self.registry_spec_path(spec_key);
        let parent = path.parent().expect("spec path has parent");
        std::fs::create_dir_all(parent)?;

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(manifest_hash.to_hex().as_bytes())?;
                file.sync_all()?;
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Lookup manifest_hash by registry spec_key (internal helper).
    fn lookup_registry_spec_local(&self, spec_key: &RegistrySpecKey) -> Option<Blake3Hash> {
        let path = self.registry_spec_path(spec_key);
        let content = std::fs::read_to_string(&path).ok()?;
        Blake3Hash::from_hex(content.trim())
    }

    /// Store a RegistryCrateManifest and return its hash.
    async fn put_registry_manifest(&self, manifest: &RegistryCrateManifest) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);

        if !path.exists() {
            let _ = self.atomic_write(&path, json.as_bytes());
        }

        hash
    }

    /// Get a RegistryCrateManifest by hash.
    pub fn get_registry_crate_manifest(
        &self,
        manifest_hash: &Blake3Hash,
    ) -> Option<RegistryCrateManifest> {
        let path = self.manifest_path(manifest_hash);
        let json = std::fs::read_to_string(&path).ok()?;
        facet_json::from_str(&json).ok()
    }
}
