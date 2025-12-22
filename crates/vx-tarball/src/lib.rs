//! Tarball extraction
//!
//! Provides safe tar.xz extraction with async interface.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum TarballError {
    #[error("xz decompression failed: {0}")]
    Decompression(#[from] std::io::Error),

    #[error("tar error: {0}")]
    Tar(String),

    #[error("non-UTF8 path in tarball")]
    NonUtf8Path,

    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create file {path}: {source}")]
    CreateFile {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write file {path}: {source}")]
    WriteFile {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    #[error("failed to set permissions on {path}: {source}")]
    SetPermissions {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    #[error("spawn_blocking failed: {0}")]
    SpawnBlocking(#[from] tokio::task::JoinError),
}

/// Extract a tar.xz archive to a destination directory.
///
/// Entries with path traversal (`..`) are silently skipped.
///
/// # Arguments
/// * `tarball_bytes` - The compressed tar.xz data
/// * `dest` - Destination directory (must exist)
/// * `strip_components` - Number of leading path components to strip (usually 1)
// TODO: extract in parallel
pub async fn extract(
    tarball_bytes: Vec<u8>,
    dest: Utf8PathBuf,
    strip_components: u32,
) -> Result<(), TarballError> {
    tokio::task::spawn_blocking(move || extract_sync(&tarball_bytes, &dest, strip_components))
        .await?
}

fn extract_sync(
    tarball_bytes: &[u8],
    dest: &Utf8Path,
    strip_components: u32,
) -> Result<(), TarballError> {
    let mut decoder = xz2::read::XzDecoder::new(tarball_bytes);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;

    let mut archive = tar::Archive::new(decompressed.as_slice());

    for entry in archive
        .entries()
        .map_err(|e| TarballError::Tar(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| TarballError::Tar(e.to_string()))?;
        let path = entry.path().map_err(|e| TarballError::Tar(e.to_string()))?;

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
        .map_err(|_| TarballError::NonUtf8Path)?;

        // Skip entries with path traversal
        let has_parent_dir = stripped_path
            .components()
            .any(|c| matches!(c, Utf8Component::ParentDir));
        if has_parent_dir {
            continue;
        }

        let target_path = dest.join(&stripped_path);

        // Create parent directories
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TarballError::CreateDir {
                path: parent.to_owned(),
                source: e,
            })?;
        }

        // Extract file
        let mut output_file =
            std::fs::File::create(&target_path).map_err(|e| TarballError::CreateFile {
                path: target_path.clone(),
                source: e,
            })?;

        std::io::copy(&mut entry, &mut output_file).map_err(|e| TarballError::WriteFile {
            path: target_path.clone(),
            source: e,
        })?;

        // Set executable bit if needed
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode() {
                if mode & 0o111 != 0 {
                    let perms = std::fs::Permissions::from_mode(mode);
                    std::fs::set_permissions(&target_path, perms).map_err(|e| {
                        TarballError::SetPermissions {
                            path: target_path.clone(),
                            source: e,
                        }
                    })?;
                }
            }
        }
    }

    Ok(())
}
