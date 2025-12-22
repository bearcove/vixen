use camino::Utf8Path;
use futures_util::StreamExt;
use tracing::debug;
use vx_cas_proto::Blake3Hash;
use vx_tarball::Compression;

use crate::ExecService;

impl ExecService {
    /// Extract a tar.xz blob from CAS to a destination directory
    pub(crate) async fn extract_tar_xz_from_cas(
        &self,
        blob_hash: Blake3Hash,
        dest: &Utf8Path,
        strip_components: u32,
    ) -> Result<(), String> {
        debug!(blob = %blob_hash, dest = %dest, strip = strip_components, "extracting tar.xz");

        // Stream blob from CAS
        let mut stream = self
            .cas
            .stream_blob(blob_hash)
            .await
            .map_err(|e| format!("failed to start blob stream: {:?}", e))?;
        let mut compressed_data = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
            compressed_data.extend_from_slice(&chunk);
        }

        vx_tarball::extract(
            compressed_data,
            dest.to_owned(),
            Compression::Xz,
            strip_components,
        )
        .await
        .map_err(|e| e.to_string())
    }
}
