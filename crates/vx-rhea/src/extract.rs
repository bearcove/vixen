use camino::Utf8Path;
use futures_util::StreamExt;
use tracing::debug;
use vx_oort_proto::{Blake3Hash, TreeManifest};

use crate::RheaService;
use crate::error::{Result, RheaError};

impl RheaService {
    /// Materialize a tree from CAS to a destination directory
    pub(crate) async fn materialize_tree_from_cas(
        &self,
        tree_manifest_hash: Blake3Hash,
        dest: &Utf8Path,
    ) -> Result<()> {
        debug!(tree = %tree_manifest_hash, dest = %dest, "materializing tree");

        // Fetch tree manifest blob from CAS
        let tree_json = self
            .cas
            .get_blob(tree_manifest_hash)
            .await
            .map_err(|e| RheaError::CasRpc(e.to_string()))?
            .ok_or(RheaError::ToolchainBlobNotFound {
                hash: tree_manifest_hash,
                path: "tree manifest".to_string(),
            })?;

        // Parse tree manifest
        let tree: TreeManifest =
            facet_json::from_str(std::str::from_utf8(&tree_json).map_err(|e| {
                RheaError::ToolchainMaterialization(format!(
                    "tree manifest is not valid UTF-8: {}",
                    e
                ))
            })?)
            .map_err(|e| {
                RheaError::ToolchainMaterialization(format!("failed to parse tree manifest: {}", e))
            })?;

        debug!(
            tree = %tree_manifest_hash,
            files = tree.entries.len(),
            total_bytes = tree.total_size_bytes,
            "parsed tree manifest"
        );

        // Materialize using vx_tarball::materialize_tree
        let cas = self.cas.clone();
        vx_tarball::materialize_tree(&tree, dest, async move |blob_hash| {
            // Fetch blob from CAS
            let mut stream = cas
                .stream_blob(blob_hash)
                .await
                .map_err(|e| format!("failed to start blob stream: {:?}", e))?;

            let mut data = Vec::new();
            while let Some(chunk_result) = stream.next().await {
                let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
                data.extend_from_slice(&chunk);
            }

            Ok(data)
        })
        .await
        .map_err(|e| {
            RheaError::ToolchainMaterialization(format!("failed to materialize tree: {}", e))
        })?;

        debug!(tree = %tree_manifest_hash, dest = %dest, "tree materialized");
        Ok(())
    }
}
