//! Registry crate materialization for execd.
//!
//! Handles extracting registry crates from CAS into the global cache
//! and copying them to workspace-local staging directories.

use std::collections::HashMap;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use futures_util::StreamExt;
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, Cas, CasClient};
use vx_cas_proto::{RegistryCrateManifest, RegistryMaterializationResult};
use vx_tarball::Compression;

/// Registry materialization manager.
///
/// Handles global cache extraction and workspace-local copying.
pub struct RegistryMaterializer {
    cas: Arc<CasClient>,

    /// Global cache directory (~/.vx/registry)
    global_cache_dir: Utf8PathBuf,

    /// In-flight global cache materializations (keyed by manifest_hash)
    materializing: Arc<
        tokio::sync::Mutex<
            HashMap<Blake3Hash, Arc<tokio::sync::OnceCell<Result<Utf8PathBuf, String>>>>,
        >,
    >,
}

impl RegistryMaterializer {
    pub fn new(cas: Arc<CasClient>, global_cache_dir: Utf8PathBuf) -> Self {
        Self {
            cas,
            global_cache_dir,
            materializing: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Materialize a registry crate to the workspace.
    ///
    /// 1. Ensure crate is in global cache (~/.vx/registry/<name>/<version>/<checksum>/)
    /// 2. Copy to workspace-local staging (<workspace>/.vx/registry/<name>/<version>/)
    /// 3. Verify or update .checksum file
    pub async fn materialize(
        &self,
        manifest_hash: Blake3Hash,
        workspace_root: &Utf8Path,
    ) -> Result<RegistryMaterializationResult, String> {
        // Fetch manifest from CAS
        let manifest = self
            .cas
            .get_registry_manifest(manifest_hash)
            .await
            .ok_or_else(|| format!("registry manifest {} not found in CAS", manifest_hash))?;

        let spec = &manifest.spec;

        // Ensure global cache extraction (deduplicated)
        let global_path = self.ensure_global_cache(&manifest).await?;

        // Workspace-local path: .vx/registry/<name>/<version>/
        let workspace_rel = format!(".vx/registry/{}/{}", spec.name, spec.version);
        let workspace_local = workspace_root.join(&workspace_rel);

        // Check if we need to re-copy (checksum mismatch or missing)
        let needs_copy = if workspace_local.exists() {
            let checksum_file = workspace_local.join(".checksum");
            match std::fs::read_to_string(&checksum_file) {
                Ok(existing_checksum) => {
                    if existing_checksum.trim() != spec.checksum {
                        info!(
                            name = %spec.name,
                            version = %spec.version,
                            "checksum mismatch, re-copying registry crate"
                        );
                        // Remove old copy
                        std::fs::remove_dir_all(&workspace_local)
                            .map_err(|e| format!("failed to remove stale workspace copy: {}", e))?;
                        true
                    } else {
                        false
                    }
                }
                Err(_) => {
                    // No checksum file, need to re-copy
                    std::fs::remove_dir_all(&workspace_local).map_err(|e| {
                        format!("failed to remove workspace copy without checksum: {}", e)
                    })?;
                    true
                }
            }
        } else {
            true
        };

        if needs_copy {
            // Create parent directory
            if let Some(parent) = workspace_local.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create workspace directory: {}", e))?;
            }

            // Use clonetree for efficient copy (reflink if supported)
            let options = clonetree::Options::default();
            clonetree::clone_tree(&global_path, &workspace_local, &options).map_err(|e| {
                format!(
                    "failed to clone {} to {}: {}",
                    global_path, workspace_local, e
                )
            })?;

            // Write .checksum file
            let checksum_file = workspace_local.join(".checksum");
            std::fs::write(&checksum_file, &spec.checksum)
                .map_err(|e| format!("failed to write checksum file: {}", e))?;

            info!(
                name = %spec.name,
                version = %spec.version,
                workspace_path = %workspace_local,
                "materialized registry crate to workspace"
            );
        } else {
            debug!(
                name = %spec.name,
                version = %spec.version,
                "registry crate already in workspace with matching checksum"
            );
        }

        Ok(RegistryMaterializationResult {
            workspace_rel_path: workspace_rel,
            global_cache_path: global_path.to_string(),
            was_cached: !needs_copy,
        })
    }

    /// Ensure a crate is extracted to the global cache.
    /// Uses deduplication to prevent concurrent extractions.
    async fn ensure_global_cache(
        &self,
        manifest: &RegistryCrateManifest,
    ) -> Result<Utf8PathBuf, String> {
        let spec = &manifest.spec;

        // Global cache path: ~/.vx/registry/<name>/<version>/<checksum>/
        let cache_path = self
            .global_cache_dir
            .join(&spec.name)
            .join(&spec.version)
            .join(&spec.checksum);

        // Check if already materialized
        let materialized_marker = cache_path.join(".materialized");
        if materialized_marker.exists() {
            debug!(
                name = %spec.name,
                version = %spec.version,
                "registry crate already in global cache"
            );
            return Ok(cache_path);
        }

        // Use inflight deduplication
        let tarball_blob = manifest.crate_tarball_blob;
        let cell = {
            let mut map = self.materializing.lock().await;
            map.entry(tarball_blob)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        cell.get_or_init(|| {
            let cache_path = cache_path.clone();
            let tarball_blob = manifest.crate_tarball_blob;
            let cas = &self.cas;

            async move {
                // Lock file for concurrent process safety
                let lock_path = cache_path
                    .parent()
                    .unwrap()
                    .join(format!(".lock.{}", &manifest.spec.checksum[..16]));

                // Create parent directory
                if let Some(parent) = lock_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create cache directory: {}", e))?;
                }

                // Simple file-based lock (create exclusive)
                let _lock = acquire_lock(&lock_path)?;

                // Check again after acquiring lock
                let materialized_marker = cache_path.join(".materialized");
                if materialized_marker.exists() {
                    return Ok(cache_path);
                }

                info!(
                    name = %manifest.spec.name,
                    version = %manifest.spec.version,
                    "extracting registry crate to global cache"
                );

                // Create temp directory for extraction
                let temp_path = cache_path.with_extension("tmp");
                if temp_path.exists() {
                    std::fs::remove_dir_all(&temp_path)
                        .map_err(|e| format!("failed to remove stale temp dir: {}", e))?;
                }
                std::fs::create_dir_all(&temp_path)
                    .map_err(|e| format!("failed to create temp dir: {}", e))?;

                // Stream and extract tarball
                extract_crate_tarball(cas, tarball_blob, &temp_path).await?;

                // Atomic rename to final location
                if cache_path.exists() {
                    std::fs::remove_dir_all(&cache_path)
                        .map_err(|e| format!("failed to remove stale cache dir: {}", e))?;
                }
                std::fs::rename(&temp_path, &cache_path)
                    .map_err(|e| format!("failed to rename to cache dir: {}", e))?;

                // Write materialized marker
                std::fs::write(&materialized_marker, "")
                    .map_err(|e| format!("failed to write materialized marker: {}", e))?;

                Ok(cache_path)
            }
        })
        .await
        .clone()
    }
}

/// Extract a .crate tarball (gzipped tar) from CAS to a destination directory.
/// Uses strip_components=1 to remove the top-level <name>-<version>/ directory.
async fn extract_crate_tarball<C: Cas>(
    cas: &C,
    blob_hash: Blake3Hash,
    dest: &Utf8Path,
) -> Result<(), String> {
    debug!(blob = %blob_hash, dest = %dest, "extracting .crate tarball");

    // Stream blob from CAS
    let mut stream = cas.stream_blob(blob_hash).await;
    let mut compressed_data = Vec::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
        compressed_data.extend_from_slice(&chunk);
    }

    // .crate files are gzipped tarballs with strip_components=1
    vx_tarball::extract(compressed_data, dest.to_owned(), Compression::Gzip, 1)
        .await
        .map_err(|e| e.to_string())
}

/// Simple file-based lock guard
struct LockGuard {
    path: Utf8PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_lock(path: &Utf8Path) -> Result<LockGuard, String> {
    use std::fs::OpenOptions;

    // Try to create the lock file exclusively
    // Retry a few times with backoff
    for attempt in 0..10 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(_) => {
                return Ok(LockGuard {
                    path: path.to_owned(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process holds the lock
                if attempt < 9 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        100 * (attempt + 1) as u64,
                    ));
                    continue;
                }
                // Check if lock is stale (older than 5 minutes)
                if let Ok(metadata) = std::fs::metadata(path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default()
                            > std::time::Duration::from_secs(300)
                        {
                            warn!(path = %path, "removing stale lock file");
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                    }
                }
                return Err(format!("failed to acquire lock {}: file exists", path));
            }
            Err(e) => {
                return Err(format!("failed to acquire lock {}: {}", path, e));
            }
        }
    }
    Err(format!("failed to acquire lock {} after retries", path))
}
