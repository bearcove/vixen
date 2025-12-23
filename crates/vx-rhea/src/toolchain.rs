use camino::Utf8PathBuf;
use tracing::{debug, info};
use vx_oort_proto::{Blake3Hash, MaterializeStep};

use crate::RheaService;
use crate::error::{Result, RheaError};

impl RheaService {
    /// Materialize a toolchain from CAS to local directory
    pub(crate) async fn materialize_toolchain_impl(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf> {
        info!(manifest_hash = %manifest_hash, "materializing toolchain");

        // Fetch manifest from CAS
        let _manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .map_err(|e| RheaError::CasRpc(e.to_string()))?
            .ok_or(RheaError::ToolchainManifestNotFound(manifest_hash))?;

        // Fetch materialization plan
        let plan = self
            .cas
            .get_materialization_plan(manifest_hash)
            .await
            .map_err(|e| RheaError::CasRpc(e.to_string()))?
            .ok_or(RheaError::ToolchainManifestNotFound(manifest_hash))?;

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
                .map_err(|e| RheaError::CreateDir {
                    path: temp_dir.clone(),
                    message: e.to_string(),
                })?;
        }
        tokio::fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| RheaError::CreateDir {
                path: temp_dir.clone(),
                message: e.to_string(),
            })?;

        // Execute materialization plan
        for step in &plan.steps {
            match step {
                MaterializeStep::EnsureDir { relpath } => {
                    let dest = temp_dir.join(relpath);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| RheaError::CreateDir {
                            path: dest.clone(),
                            message: e.to_string(),
                        })?;
                }
                MaterializeStep::Symlink { relpath, target } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            RheaError::CreateDir {
                                path: parent.to_owned(),
                                message: e.to_string(),
                            }
                        })?;
                    }
                    #[cfg(unix)]
                    {
                        tokio::fs::symlink(target, &dest).await.map_err(|e| {
                            RheaError::WriteFile {
                                path: dest.clone(),
                                message: format!("symlink to {}: {}", target, e),
                            }
                        })?;
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(RheaError::SymlinksNotSupported);
                    }
                }
                MaterializeStep::MaterializeTree {
                    tree_manifest,
                    dest_subdir,
                } => {
                    let dest = temp_dir.join(dest_subdir);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| RheaError::CreateDir {
                            path: dest.clone(),
                            message: e.to_string(),
                        })?;

                    self.materialize_tree_from_cas(*tree_manifest, &dest)
                        .await?;
                }
            }
        }

        // Atomic rename to final location
        tokio::fs::rename(&temp_dir, &target_dir)
            .await
            .map_err(|e| {
                RheaError::ToolchainMaterialization(format!(
                    "failed to rename {} to {}: {}",
                    temp_dir, target_dir, e
                ))
            })?;

        info!(target_dir = %target_dir, "toolchain materialized successfully");
        Ok(target_dir)
    }
}
