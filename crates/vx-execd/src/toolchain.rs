use camino::Utf8PathBuf;
use tracing::{debug, info};
use vx_cas_proto::{Blake3Hash, MaterializeStep};

use crate::ExecService;

impl ExecService {
    /// Materialize a toolchain from CAS to local directory
    pub(crate) async fn materialize_toolchain_impl(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
        info!(manifest_hash = %manifest_hash, "materializing toolchain");

        // Fetch manifest from CAS
        let _manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .map_err(|e| format!("failed to fetch toolchain manifest: {:?}", e))?
            .ok_or_else(|| format!("toolchain manifest {} not found in CAS", manifest_hash))?;

        // Fetch materialization plan
        let plan = self
            .cas
            .get_materialization_plan(manifest_hash)
            .await
            .map_err(|e| format!("failed to fetch materialization plan: {:?}", e))?
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
                MaterializeStep::EnsureDir { relpath } => {
                    let dest = temp_dir.join(relpath);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create directory {}: {}", dest, e))?;
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
                        return Err("symlinks not supported on this platform".to_string());
                    }
                }
                MaterializeStep::MaterializeTree {
                    tree_manifest,
                    dest_subdir,
                } => {
                    let dest = temp_dir.join(dest_subdir);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create dest dir {}: {}", dest, e))?;

                    self.materialize_tree_from_cas(*tree_manifest, &dest)
                        .await?;
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
