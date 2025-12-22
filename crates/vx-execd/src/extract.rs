use camino::{Utf8Path, Utf8PathBuf};
use futures_util::StreamExt;
use std::io::Read as _;
use tracing::debug;
use vx_cas_proto::Blake3Hash;

use crate::ExecService;

impl ExecService {
    /// Extract a tar.xz blob from CAS to a destination directory
    async fn extract_tar_xz_from_cas(
        &self,
        blob_hash: Blake3Hash,
        dest: &Utf8Path,
        strip_components: u32,
    ) -> Result<(), String> {
        debug!(blob = %blob_hash, dest = %dest, strip = strip_components, "extracting tar.xz");

        // Stream blob from CAS
        let mut stream = self.cas.stream_blob(blob_hash).await;
        let mut compressed_data = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
            compressed_data.extend_from_slice(&chunk);
        }

        // CPU-bound: decompress and extract in spawn_blocking
        let dest = dest.to_owned();
        tokio::task::spawn_blocking(move || {
            // Decompress with xz2
            let mut decompressor = xz2::read::XzDecoder::new(&compressed_data[..]);
            let mut tarball_data = Vec::new();
            decompressor
                .read_to_end(&mut tarball_data)
                .map_err(|e| format!("failed to decompress tar.xz: {}", e))?;

            // Extract tar
            let mut archive = tar::Archive::new(&tarball_data[..]);
            for entry in archive
                .entries()
                .map_err(|e| format!("failed to read tar entries: {}", e))?
            {
                let mut entry = entry.map_err(|e| format!("failed to read tar entry: {}", e))?;
                let path = entry
                    .path()
                    .map_err(|e| format!("failed to get entry path: {}", e))?;

                // Strip components
                let components: Vec<_> = path.components().collect();
                if components.len() <= strip_components as usize {
                    continue;
                }
                let stripped_path = Utf8PathBuf::from_path_buf(
                    components[strip_components as usize..]
                        .iter()
                        .collect::<std::path::PathBuf>(),
                )
                .map_err(|_| "non-UTF8 path in tarball".to_string())?;

                // Security: validate no path traversal (use component-based check to avoid
                // false positives on valid filenames like "foo..rs")
                for component in stripped_path.components() {
                    if matches!(component, camino::Utf8Component::ParentDir) {
                        return Err(format!("path traversal in tarball: {}", stripped_path));
                    }
                }

                let target_path = dest.join(&stripped_path);

                // Create parent directories
                if let Some(parent) = target_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create directory {}: {}", parent, e))?;
                }

                // Extract file
                let mut output_file = std::fs::File::create(&target_path)
                    .map_err(|e| format!("failed to create file {}: {}", target_path, e))?;
                std::io::copy(&mut entry, &mut output_file)
                    .map_err(|e| format!("failed to write file {}: {}", target_path, e))?;

                // Set executable bit if needed
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = entry.header().mode() {
                        if mode & 0o111 != 0 {
                            let perms = std::fs::Permissions::from_mode(mode);
                            std::fs::set_permissions(&target_path, perms).map_err(|e| {
                                format!("failed to set permissions on {}: {}", target_path, e)
                            })?;
                        }
                    }
                }
            }

            Ok(())
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
    }
}
