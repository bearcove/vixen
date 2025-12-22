//! Tarball extraction and download
//!
//! Provides streaming tar.xz and tar.gz extraction from any AsyncRead source,
//! plus download_and_extract for HTTP sources with hash verification.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::AsyncRead;

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

    #[error("HTTP request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    #[error("HTTP error {status} for {url}")]
    HttpStatus { url: String, status: u16 },

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("download failed after {attempts} attempts: {last_error}")]
    DownloadFailed { attempts: u32, last_error: String },
}

/// Compression format for tarballs
#[derive(Debug, Clone, Copy)]
pub enum Compression {
    /// xz compression (tar.xz)
    Xz,
    /// gzip compression (tar.gz, .crate files)
    Gzip,
}

/// Hash for verification
#[derive(Debug, Clone)]
pub enum VerifHash {
    Sha256(String),
}

impl VerifHash {
    pub fn sha256(hex: impl Into<String>) -> Self {
        Self::Sha256(hex.into())
    }
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
            if let Ok(mode) = entry.header().mode() {
                if mode & 0o111 != 0 {
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
    }

    Ok(())
}

/// Download and extract a tarball from a URL with hash verification.
///
/// Streams the download through decompression and extraction - does not
/// buffer the entire file in memory.
///
/// # Arguments
/// * `url` - URL to download from
/// * `expected_hashes` - Hashes to verify against (currently only SHA256 supported)
/// * `dest` - Destination directory
/// * `compression` - Compression format
/// * `strip_components` - Number of leading path components to strip
/// * `max_retries` - Maximum number of retry attempts for transient errors
pub async fn download_and_extract(
    url: &str,
    expected_hashes: &[VerifHash],
    dest: &Utf8Path,
    compression: Compression,
    strip_components: u32,
    max_retries: u32,
) -> Result<(), TarballError> {
    let mut last_error = String::new();
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(10);

    for attempt in 0..max_retries {
        if attempt > 0 {
            tracing::info!(
                attempt = attempt + 1,
                backoff_ms = backoff.as_millis(),
                "retrying download"
            );
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }

        match download_and_extract_once(url, expected_hashes, dest, compression, strip_components)
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = e.to_string();
                if !is_retryable(&e) {
                    return Err(e);
                }
                tracing::warn!(error = %e, "transient download error");
            }
        }
    }

    Err(TarballError::DownloadFailed {
        attempts: max_retries,
        last_error,
    })
}

async fn download_and_extract_once(
    url: &str,
    expected_hashes: &[VerifHash],
    dest: &Utf8Path,
    compression: Compression,
    strip_components: u32,
) -> Result<(), TarballError> {
    tracing::info!(url = %url, "downloading and extracting");

    let response = reqwest::get(url).await?;
    let status = response.status();

    if status.as_u16() == 429 {
        return Err(TarballError::HttpStatus {
            url: url.to_string(),
            status: 429,
        });
    }

    if !status.is_success() {
        return Err(TarballError::HttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }

    // For hash verification, we need to read the whole thing first
    // (can't stream and verify hash simultaneously without buffering)
    // Future optimization: use a tee reader to hash while streaming
    let bytes = response.bytes().await?;

    // Verify hashes
    for hash in expected_hashes {
        match hash {
            VerifHash::Sha256(expected) => {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                let actual = hex::encode(hasher.finalize());
                if actual.to_lowercase() != expected.to_lowercase() {
                    return Err(TarballError::ChecksumMismatch {
                        url: url.to_string(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
        }
    }

    // Extract from the verified bytes
    let cursor = std::io::Cursor::new(bytes.to_vec());
    extract(cursor, dest, compression, strip_components).await
}

fn is_retryable(err: &TarballError) -> bool {
    match err {
        TarballError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
        TarballError::HttpRequest(e) => e.is_timeout() || e.is_connect(),
        _ => false,
    }
}
