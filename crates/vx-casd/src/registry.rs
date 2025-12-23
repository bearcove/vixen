//! Registry crate acquisition for casd.
//!
//! Downloads .crate tarballs from crates.io, validates checksums and structure,
//! and stores them in CAS.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use thiserror::Error;
use vx_cas_proto::Blake3Hash;
use vx_cas_proto::{
    EnsureRegistryCrateResult, EnsureStatus, RegistryCrateManifest, RegistrySpecKey,
};

use crate::CasService;
use vx_io::atomic_write;

/// Maximum retry attempts for transient failures
const MAX_RETRIES: u32 = 3;

/// Initial backoff duration
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Maximum backoff duration
const MAX_BACKOFF: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
enum DownloadError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] crate::http::HttpError),

    #[error("rate limited (429)")]
    RateLimited,

    #[error("server error: {0}")]
    ServerError(u16),

    #[error("HTTP error: {0}")]
    HttpError(u16),

    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
}

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
    pub async fn ensure(
        &self,
        spec_key: RegistrySpecKey,
        lookup_fn: impl AsyncFn() -> Option<Blake3Hash> + 'static,
        acquire_fn: impl AsyncFn() -> EnsureRegistryCrateResult + 'static,
    ) -> EnsureRegistryCrateResult {
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
pub(crate) async fn download_crate(
    name: &str,
    version: &str,
    expected_checksum: &str,
) -> Result<Vec<u8>, String> {
    let url = format!(
        "https://crates.io/api/v1/crates/{}/{}/download",
        name, version
    );

    let mut last_error: Option<DownloadError> = None;
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
                // Check if error is retryable
                if !is_retryable_error(&e) {
                    return Err(e.to_string());
                }
                tracing::warn!(error = %e, "transient download error");
                last_error = Some(e);
            }
        }
    }

    Err(format!(
        "download failed after {} attempts: {}",
        MAX_RETRIES,
        last_error.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Single download attempt with streaming verification
async fn download_crate_attempt(url: &str, expected_checksum: &str) -> Result<Vec<u8>, DownloadError> {
    use http_body_util::BodyExt;
    use tokio::io::AsyncReadExt;

    tracing::info!(url = %url, "downloading crate");

    let response = crate::http::get(url).await?;
    let status = response.status();

    // Check for rate limiting
    if status.as_u16() == 429 {
        return Err(DownloadError::RateLimited);
    }

    // Check for server errors (retryable)
    if status.is_server_error() {
        return Err(DownloadError::ServerError(status.as_u16()));
    }

    // Check for client errors (not retryable except 429)
    if !status.is_success() {
        return Err(DownloadError::HttpError(status.as_u16()));
    }

    // Wrap the response body in a hash-verifying reader
    let body_reader = tokio_util::io::StreamReader::new(
        response.into_body().into_data_stream().map(|result| {
            result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }),
    );

    let mut verifying_reader = crate::hash_reader::Sha256VerifyingReader::new(
        body_reader,
        expected_checksum.to_string(),
    );

    // Read through the verifying reader (hash verification happens on EOF)
    let mut bytes = Vec::new();
    verifying_reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| DownloadError::ChecksumMismatch {
            expected: expected_checksum.to_string(),
            actual: e.to_string(),
        })?;

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        checksum = %expected_checksum,
        "crate downloaded and verified"
    );

    Ok(bytes)
}

fn is_retryable_error(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::RateLimited | DownloadError::ServerError(_) | DownloadError::Http(_)
    )
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
    /// Publish registry spec → manifest_hash mapping atomically (first-writer-wins).
    pub(crate) async fn publish_registry_spec_mapping(
        &self,
        spec_key: &RegistrySpecKey,
        manifest_hash: &Blake3Hash,
    ) -> std::io::Result<bool> {
        let path = self.registry_spec_path(spec_key);
        let parent = path.parent().expect("spec path has parent").to_path_buf();
        let manifest_hex = manifest_hash.to_hex();

        tokio::task::spawn_blocking(move || {
            use std::fs::{File, OpenOptions};
            use std::io::Write;

            std::fs::create_dir_all(&parent)?;

            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(manifest_hex.as_bytes())?;
                    file.sync_all()?;
                    if let Ok(dir) = File::open(&parent) {
                        let _ = dir.sync_all();
                    }
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        })
        .await
        .expect("spawn_blocking join failed")
    }

    /// Lookup manifest_hash by registry spec_key (internal helper).
    pub(crate) async fn lookup_registry_spec_local(
        &self,
        spec_key: &RegistrySpecKey,
    ) -> Option<Blake3Hash> {
        let path = self.registry_spec_path(spec_key);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        Blake3Hash::from_hex(content.trim())
    }

    /// Store a RegistryCrateManifest and return its hash.
    pub(crate) async fn put_registry_manifest(
        &self,
        manifest: &RegistryCrateManifest,
    ) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);

        if !path.exists() {
            if let Err(e) = atomic_write(&path, json.as_bytes()).await {
                tracing::warn!("failed to write registry manifest to {}: {}", path, e);
            }
        }

        hash
    }

    /// Get a RegistryCrateManifest by hash.
    pub async fn get_registry_crate_manifest(
        &self,
        manifest_hash: &Blake3Hash,
    ) -> Option<RegistryCrateManifest> {
        let path = self.manifest_path(manifest_hash);
        let json = tokio::fs::read_to_string(&path).await.ok()?;
        facet_json::from_str(&json).ok()
    }
}
