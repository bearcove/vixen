//! Tarball extraction
//!
//! Provides safe tar.xz and tar.gz extraction with async interface.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum TarballError {
    #[error("decompression failed: {0}")]
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

/// Compression format for tarballs
#[derive(Debug, Clone, Copy)]
pub enum Compression {
    /// xz compression (tar.xz)
    Xz,
    /// gzip compression (tar.gz, .crate files)
    Gzip,
}

/// Extract a tarball to a destination directory.
///
/// Entries with path traversal (`..`) are silently skipped.
///
/// # Arguments
/// * `tarball_bytes` - The compressed tarball data
/// * `dest` - Destination directory (must exist)
/// * `compression` - Compression format (Xz or Gzip)
/// * `strip_components` - Number of leading path components to strip (usually 1)
// TODO: extract in parallel
pub async fn extract(
    tarball_bytes: Vec<u8>,
    dest: Utf8PathBuf,
    compression: Compression,
    strip_components: u32,
) -> Result<(), TarballError> {
    tokio::task::spawn_blocking(move || {
        extract_sync(&tarball_bytes, &dest, compression, strip_components)
    })
    .await?
}

fn extract_sync(
    tarball_bytes: &[u8],
    dest: &Utf8Path,
    compression: Compression,
    strip_components: u32,
) -> Result<(), TarballError> {
    let decompressed = decompress(tarball_bytes, compression)?;
    extract_tar(&decompressed, dest, strip_components)
}

fn decompress(data: &[u8], compression: Compression) -> Result<Vec<u8>, TarballError> {
    let mut decompressed = Vec::new();
    match compression {
        Compression::Xz => {
            let mut decoder = xz2::read::XzDecoder::new(data);
            decoder.read_to_end(&mut decompressed)?;
        }
        Compression::Gzip => {
            let mut decoder = flate2::read::GzDecoder::new(data);
            decoder.read_to_end(&mut decompressed)?;
        }
    }
    Ok(decompressed)
}

fn extract_tar(
    tarball_data: &[u8],
    dest: &Utf8Path,
    strip_components: u32,
) -> Result<(), TarballError> {
    let mut archive = tar::Archive::new(tarball_data);

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

        // Handle directories
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target_path).map_err(|e| TarballError::CreateDir {
                path: target_path.clone(),
                source: e,
            })?;
            continue;
        }

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
