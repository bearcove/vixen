//! Tarball extraction
//!
//! Provides streaming tar.xz and tar.gz extraction from any AsyncRead source.
//!
//! ## Tree extraction (CAS deduplication)
//!
//! For content-addressed storage, use [`extract_to_tree`] to extract a tarball
//! into a [`TreeManifest`], storing each file as a separate blob. This enables:
//! - Deduplication across toolchain versions
//! - Deduplication across crates
//! - Efficient partial materialization

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt};
use vx_cas_proto::{Blake3Hash, TreeManifest};

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

    #[error("failed to store blob: {0}")]
    BlobStore(String),

    #[error("failed to read entry data: {0}")]
    ReadEntry(String),
}

/// Compression format for tarballs
#[derive(Debug, Clone, Copy)]
pub enum Compression {
    /// xz compression (tar.xz)
    Xz,
    /// gzip compression (tar.gz, .crate files)
    Gzip,
}

/// Extract a tarball from any AsyncRead source.
///
/// Entries with path traversal (`..`) are silently skipped.
///
/// # Arguments
/// * `reader` - Any async reader (HTTP response, CAS stream, file, etc.)
/// * `dest` - Destination directory
/// * `compression` - Compression format (Xz or Gzip)
/// * `strip_components` - Number of leading path components to strip
// TODO: extract in parallel
pub async fn extract<R: AsyncRead + Unpin>(
    reader: R,
    dest: &Utf8Path,
    compression: Compression,
    strip_components: u32,
) -> Result<(), TarballError> {
    // Create decompressor based on compression type
    match compression {
        Compression::Xz => {
            let decoder = async_compression::tokio::bufread::XzDecoder::new(
                tokio::io::BufReader::new(reader),
            );
            extract_tar(decoder, dest, strip_components).await
        }
        Compression::Gzip => {
            let decoder = async_compression::tokio::bufread::GzipDecoder::new(
                tokio::io::BufReader::new(reader),
            );
            extract_tar(decoder, dest, strip_components).await
        }
    }
}

async fn extract_tar<R: AsyncRead + Unpin>(
    reader: R,
    dest: &Utf8Path,
    strip_components: u32,
) -> Result<(), TarballError> {
    let mut archive = tokio_tar::Archive::new(reader);
    let mut entries = archive
        .entries()
        .map_err(|e| TarballError::Tar(e.to_string()))?;

    use futures_util::StreamExt;
    while let Some(entry_result) = entries.next().await {
        let mut entry = entry_result.map_err(|e| TarballError::Tar(e.to_string()))?;

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
            tokio::fs::create_dir_all(&target_path)
                .await
                .map_err(|e| TarballError::CreateDir {
                    path: target_path.clone(),
                    source: e,
                })?;
            continue;
        }

        // Create parent directories
        if let Some(parent) = target_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| TarballError::CreateDir {
                    path: parent.to_owned(),
                    source: e,
                })?;
        }

        // Extract file
        let mut output_file =
            tokio::fs::File::create(&target_path)
                .await
                .map_err(|e| TarballError::CreateFile {
                    path: target_path.clone(),
                    source: e,
                })?;

        tokio::io::copy(&mut entry, &mut output_file)
            .await
            .map_err(|e| TarballError::WriteFile {
                path: target_path.clone(),
                source: e,
            })?;

        // Set executable bit if needed
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mode) = entry.header().mode()
                && mode & 0o111 != 0 {
                    let perms = std::fs::Permissions::from_mode(mode);
                    tokio::fs::set_permissions(&target_path, perms)
                        .await
                        .map_err(|e| TarballError::SetPermissions {
                            path: target_path.clone(),
                            source: e,
                        })?;
                }
        }
    }

    Ok(())
}

// =============================================================================
// Tree Extraction (CAS Deduplication)
// =============================================================================

/// Extract a tarball to a TreeManifest, storing each file via a callback.
///
/// Instead of writing files to disk, this function:
/// 1. Reads each file from the tarball
/// 2. Calls `store_blob` to store the file contents (e.g., in CAS)
/// 3. Records the path → blob mapping in a TreeManifest
///
/// The callback receives the file contents and should return the Blake3Hash
/// of the stored blob.
///
/// # Arguments
/// * `reader` - Any async reader (HTTP response, CAS stream, file, etc.)
/// * `compression` - Compression format (Xz or Gzip)
/// * `strip_components` - Number of leading path components to strip
/// * `store_blob` - Async callback to store blob contents, returns Blake3Hash
///
/// # Returns
/// A TreeManifest containing all extracted entries
pub async fn extract_to_tree(
    reader: impl AsyncRead + Unpin,
    compression: Compression,
    strip_components: u32,
    store_blob: impl AsyncFn(Vec<u8>) -> Result<Blake3Hash, String>,
) -> Result<TreeManifest, TarballError> {
    match compression {
        Compression::Xz => {
            let decoder = async_compression::tokio::bufread::XzDecoder::new(
                tokio::io::BufReader::new(reader),
            );
            extract_tar_to_tree(decoder, strip_components, store_blob).await
        }
        Compression::Gzip => {
            let decoder = async_compression::tokio::bufread::GzipDecoder::new(
                tokio::io::BufReader::new(reader),
            );
            extract_tar_to_tree(decoder, strip_components, store_blob).await
        }
    }
}

async fn extract_tar_to_tree(
    reader: impl AsyncRead + Unpin,
    strip_components: u32,
    store_blob: impl AsyncFn(Vec<u8>) -> Result<Blake3Hash, String>,
) -> Result<TreeManifest, TarballError> {
    let mut manifest = TreeManifest::new();
    let mut archive = tokio_tar::Archive::new(reader);
    let mut entries = archive
        .entries()
        .map_err(|e| TarballError::Tar(e.to_string()))?;

    // Track non-empty directories (we only need to record empty ones)
    let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

    use futures_util::StreamExt;
    while let Some(entry_result) = entries.next().await {
        let mut entry = entry_result.map_err(|e| TarballError::Tar(e.to_string()))?;

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

        // Use forward slashes for consistency
        let path_str = stripped_path.as_str().replace('\\', "/");

        // Mark parent directories as non-empty
        if let Some(parent) = stripped_path.parent() {
            let parent_str = parent.as_str().replace('\\', "/");
            if !parent_str.is_empty() {
                seen_dirs.insert(parent_str);
            }
        }

        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            // We'll handle empty dirs at the end
            continue;
        }

        if entry_type.is_symlink() {
            // Handle symlinks
            let target = entry
                .link_name()
                .map_err(|e| TarballError::Tar(e.to_string()))?
                .ok_or_else(|| TarballError::Tar("symlink without target".to_string()))?;

            let target_str = target
                .to_str()
                .ok_or(TarballError::NonUtf8Path)?
                .to_string();

            manifest.add_symlink(path_str, target_str);
            continue;
        }

        if entry_type.is_file() || entry_type.is_hard_link() {
            // Read file contents
            let size = entry.header().size().unwrap_or(0);
            let mut contents = Vec::with_capacity(size as usize);
            entry
                .read_to_end(&mut contents)
                .await
                .map_err(|e| TarballError::ReadEntry(e.to_string()))?;

            let actual_size = contents.len() as u64;

            // Check if executable
            let executable = entry
                .header()
                .mode()
                .map(|m| m & 0o111 != 0)
                .unwrap_or(false);

            // Store the blob via callback
            let blob_hash = store_blob(contents)
                .await
                .map_err(TarballError::BlobStore)?;

            manifest.add_file(path_str, blob_hash, actual_size, executable);
        }
        // Skip other entry types (devices, fifos, etc.)
    }

    // Finalize: sort entries and compute stats
    manifest.finalize();

    Ok(manifest)
}

// =============================================================================
// Tree Materialization
// =============================================================================

/// Materialize a TreeManifest to disk, fetching blobs via a callback.
///
/// This function:
/// 1. Creates directories as needed
/// 2. Fetches each blob via the callback
/// 3. Writes files with correct permissions
/// 4. Creates symlinks
///
/// # Arguments
/// * `manifest` - The TreeManifest to materialize
/// * `dest` - Destination directory
/// * `get_blob` - Async callback to fetch blob contents by hash
pub async fn materialize_tree(
    manifest: &TreeManifest,
    dest: &Utf8Path,
    get_blob: impl AsyncFn(Blake3Hash) -> Result<Vec<u8>, String>,
) -> Result<(), TarballError> {
    use vx_cas_proto::TreeEntryKind;

    // Create destination directory
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| TarballError::CreateDir {
            path: dest.to_owned(),
            source: e,
        })?;

    // First pass: create all directories (including empty ones)
    for path in manifest.directories() {
        let dir_path = dest.join(path);
        tokio::fs::create_dir_all(&dir_path)
            .await
            .map_err(|e| TarballError::CreateDir {
                path: dir_path,
                source: e,
            })?;
    }

    // Second pass: write all files
    for entry in &manifest.entries {
        let target_path = dest.join(&entry.path);

        match &entry.kind {
            TreeEntryKind::File {
                blob, executable, ..
            } => {
                // Ensure parent directory exists
                if let Some(parent) = target_path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        TarballError::CreateDir {
                            path: parent.to_owned(),
                            source: e,
                        }
                    })?;
                }

                // Fetch blob contents
                let contents = get_blob(*blob)
                    .await
                    .map_err(TarballError::BlobStore)?;

                // Write file
                tokio::fs::write(&target_path, &contents)
                    .await
                    .map_err(|e| TarballError::WriteFile {
                        path: target_path.clone(),
                        source: e,
                    })?;

                // Set executable bit if needed
                #[cfg(unix)]
                if *executable {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o755);
                    tokio::fs::set_permissions(&target_path, perms)
                        .await
                        .map_err(|e| TarballError::SetPermissions {
                            path: target_path.clone(),
                            source: e,
                        })?;
                }
            }

            TreeEntryKind::Symlink { target } => {
                // Ensure parent directory exists
                if let Some(parent) = target_path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        TarballError::CreateDir {
                            path: parent.to_owned(),
                            source: e,
                        }
                    })?;
                }

                // Remove existing file/symlink if present
                if let Err(e) = tokio::fs::remove_file(&target_path).await {
                    // NotFound is expected if the file doesn't exist
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("Failed to remove existing file at {}: {e}", target_path);
                    }
                }

                // Create symlink
                #[cfg(unix)]
                {
                    tokio::fs::symlink(target, &target_path)
                        .await
                        .map_err(|e| TarballError::WriteFile {
                            path: target_path.clone(),
                            source: e,
                        })?;
                }

                #[cfg(windows)]
                {
                    // On Windows, try file symlink first
                    tokio::fs::symlink_file(target, &target_path)
                        .await
                        .map_err(|e| TarballError::WriteFile {
                            path: target_path.clone(),
                            source: e,
                        })?;
                }
            }

            TreeEntryKind::Directory => {
                // Already handled in first pass
            }
        }
    }

    Ok(())
}
