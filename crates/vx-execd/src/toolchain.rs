use camino::Utf8PathBuf;
use futures_util::StreamExt;
use tracing::{debug, info};
use vx_cas_proto::{Blake3Hash, MaterializeStep};

use crate::ExecService;

impl ExecService {
    /// Materialize a toolchain from CAS to local directory
    async fn materialize_toolchain_impl(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
        info!(manifest_hash = %manifest_hash, "materializing toolchain");

        // Fetch manifest from CAS
        let _manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .ok_or_else(|| format!("toolchain manifest {} not found in CAS", manifest_hash))?;

        // Fetch materialization plan
        let plan = self
            .cas
            .get_materialization_plan(manifest_hash)
            .await
            .ok_or_else(|| {
                format!(
                    "materialization plan for {} not found in CAS",
                    manifest_hash
                )
            })?;

        // Target directory: toolchains/<manifest_hash_hex>
        let target_dir = self.toolchains_dir.join(manifest_hash.to_hex());

        // Check if already materialized (using tokio::fs)
        if tokio::fs::try_exists(&target_dir).await.unwrap_or(false) {
            debug!(target_dir = %target_dir, "toolchain already materialized");
            return Ok(target_dir);
        }

        // Create temp directory for atomic materialization
        let temp_dir = self
            .toolchains_dir
            .join(format!("{}.tmp", manifest_hash.to_hex()));
        if tokio::fs::try_exists(&temp_dir).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&temp_dir)
                .await
                .map_err(|e| format!("failed to remove stale temp dir {}: {}", temp_dir, e))?;
        }
        tokio::fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| format!("failed to create temp dir {}: {}", temp_dir, e))?;

        // Execute materialization plan
        for step in &plan.steps {
            match step {
                MaterializeStep::ExtractTarXz {
                    blob,
                    dest_subdir,
                    strip_components,
                } => {
                    let dest = temp_dir.join(dest_subdir);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create dest dir {}: {}", dest, e))?;

                    self.extract_tar_xz_from_cas(*blob, &dest, *strip_components)
                        .await?;
                }
                MaterializeStep::EnsureDir { relpath } => {
                    let dest = temp_dir.join(relpath);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create directory {}: {}", dest, e))?;
                }
                MaterializeStep::WriteFile {
                    relpath,
                    blob,
                    mode,
                } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }

                    // Fetch blob from CAS
                    let mut stream = self.cas.stream_blob(*blob).await;
                    let mut data = Vec::new();
                    while let Some(chunk_result) = stream.next().await {
                        let chunk =
                            chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
                        data.extend_from_slice(&chunk);
                    }

                    tokio::fs::write(&dest, data)
                        .await
                        .map_err(|e| format!("failed to write file {}: {}", dest, e))?;

                    // Set permissions
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(*mode);
                        tokio::fs::set_permissions(&dest, perms)
                            .await
                            .map_err(|e| format!("failed to set permissions on {}: {}", dest, e))?;
                    }
                }
                MaterializeStep::Symlink { relpath, target } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }
                    #[cfg(unix)]
                    {
                        tokio::fs::symlink(target, &dest)
                            .await
                            .map_err(|e| format!("failed to create symlink {}: {}", dest, e))?;
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(format!("symlinks not supported on this platform"));
                    }
                }
            }
        }

        // Atomic rename to final location
        tokio::fs::rename(&temp_dir, &target_dir)
            .await
            .map_err(|e| format!("failed to rename {} to {}: {}", temp_dir, target_dir, e))?;

        info!(target_dir = %target_dir, "toolchain materialized successfully");
        Ok(target_dir)
    }
}
