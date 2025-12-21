//! Zig toolchain acquisition and management
//!
//! Downloads and manages Zig toolchains for hermetic C/C++ compilation.
//! Uses `zig cc` as a drop-in hermetic C/C++ compiler.

use std::io::Write;

use camino::{Utf8Path, Utf8PathBuf};
use thiserror::Error;
use vx_cas_proto::Blake3Hash;

/// Errors that can occur during Zig toolchain operations
#[derive(Debug, Error)]
pub enum ZigError {
    #[error("failed to fetch from {url}: {source}")]
    FetchError {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("unsupported host platform: {os}-{arch}")]
    UnsupportedPlatform { os: String, arch: String },

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("zig executable not found in archive")]
    ZigNotFound,

    #[error("zig lib directory not found in archive")]
    LibNotFound,
}

/// Zig version specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ZigVersion {
    /// Version string (e.g., "0.13.0", "0.14.0-dev.1234+abcdef")
    pub version: String,
}

impl ZigVersion {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }

    /// Check if this is a release version (not dev/nightly)
    pub fn is_release(&self) -> bool {
        !self.version.contains("-dev")
    }
}

impl std::fmt::Display for ZigVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.version)
    }
}

/// Host platform for the Zig toolchain
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPlatform {
    /// OS: "linux", "macos", "windows"
    pub os: String,
    /// Architecture: "x86_64", "aarch64"
    pub arch: String,
}

impl HostPlatform {
    /// Detect the current host platform
    pub fn detect() -> Result<Self, ZigError> {
        let os = match std::env::consts::OS {
            "linux" => "linux",
            "macos" => "macos",
            "windows" => "windows",
            other => {
                return Err(ZigError::UnsupportedPlatform {
                    os: other.to_string(),
                    arch: std::env::consts::ARCH.to_string(),
                });
            }
        };

        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            other => {
                return Err(ZigError::UnsupportedPlatform {
                    os: os.to_string(),
                    arch: other.to_string(),
                });
            }
        };

        Ok(Self {
            os: os.to_string(),
            arch: arch.to_string(),
        })
    }

    /// Get the Zig download filename component for this platform
    pub fn zig_platform_name(&self) -> String {
        format!("{}-{}", self.arch, self.os)
    }
}

impl std::fmt::Display for HostPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.arch, self.os)
    }
}

/// A downloaded and verified Zig toolchain
#[derive(Debug, Clone)]
pub struct ZigToolchain {
    /// Unique identifier for this toolchain (hash of all files)
    pub id: ToolchainId,
    /// Zig version
    pub version: ZigVersion,
    /// Host platform (what platform zig runs on)
    pub host: HostPlatform,
    /// Hash of the zig executable
    pub zig_exe_hash: Blake3Hash,
    /// Hash of the lib directory (as a tarball)
    pub lib_hash: Blake3Hash,
}

/// Unique identifier for a toolchain, derived from its contents
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolchainId(pub Blake3Hash);

impl ToolchainId {
    /// Create a ToolchainId from zig exe and lib hashes
    pub fn from_parts(zig_exe_hash: &Blake3Hash, lib_hash: &Blake3Hash) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&zig_exe_hash.0);
        hasher.update(&lib_hash.0);
        Self(Blake3Hash(*hasher.finalize().as_bytes()))
    }
}

impl std::fmt::Display for ToolchainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zig:{}", self.0.to_hex())
    }
}

/// Build the download URL for a Zig release
pub fn zig_download_url(version: &ZigVersion, platform: &HostPlatform) -> String {
    let ext = if platform.os == "windows" {
        "zip"
    } else {
        "tar.xz"
    };

    // Official Zig releases are at ziglang.org/download/{version}/
    format!(
        "https://ziglang.org/download/{}/zig-{}-{}.{}",
        version.version,
        platform.zig_platform_name(),
        version.version,
        ext
    )
}

/// Download a Zig toolchain tarball
pub async fn download_zig(
    version: &ZigVersion,
    platform: &HostPlatform,
) -> Result<Vec<u8>, ZigError> {
    let url = zig_download_url(version, platform);

    tracing::info!(url = %url, "downloading Zig toolchain");

    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(ZigError::FetchError {
            url: url.clone(),
            source: format!("HTTP {}", response.status()).into(),
        });
    }

    let bytes = response.bytes().await?;

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        "downloaded Zig tarball"
    );

    Ok(bytes.to_vec())
}

/// Extract a Zig tarball and return paths to the zig executable and lib directory
///
/// Returns (zig_exe_path, lib_dir_path) within the extraction directory
pub fn extract_zig_tarball(
    tarball: &[u8],
    extract_dir: &Utf8Path,
) -> Result<(Utf8PathBuf, Utf8PathBuf), ZigError> {
    use std::fs;
    use std::io::Read;
    use xz2::read::XzDecoder;

    // Create extraction directory
    fs::create_dir_all(extract_dir)?;

    // Decompress xz
    let mut decoder = XzDecoder::new(tarball);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| ZigError::ExtractionFailed(format!("xz decompression failed: {}", e)))?;

    // Extract tar
    let mut archive = tar::Archive::new(decompressed.as_slice());

    // Extract all entries
    archive
        .unpack(extract_dir)
        .map_err(|e| ZigError::ExtractionFailed(format!("tar extraction failed: {}", e)))?;

    // Find the zig directory (it's usually zig-{platform}-{version}/)
    let mut zig_dir: Option<Utf8PathBuf> = None;
    for entry in fs::read_dir(extract_dir)? {
        let entry = entry?;
        let path = Utf8PathBuf::try_from(entry.path())
            .map_err(|e| ZigError::ExtractionFailed(format!("non-UTF8 path: {}", e)))?;
        if path.file_name().map_or(false, |n| n.starts_with("zig-")) {
            zig_dir = Some(path);
            break;
        }
    }

    let zig_dir = zig_dir.ok_or(ZigError::ZigNotFound)?;

    // Locate zig executable
    let zig_exe = zig_dir.join("zig");
    if !zig_exe.exists() {
        return Err(ZigError::ZigNotFound);
    }

    // Locate lib directory
    let lib_dir = zig_dir.join("lib");
    if !lib_dir.exists() {
        return Err(ZigError::LibNotFound);
    }

    Ok((zig_exe, lib_dir))
}

/// Hash a file and return its Blake3 hash
pub fn hash_file(path: &Utf8Path) -> Result<Blake3Hash, ZigError> {
    let contents = std::fs::read(path)?;
    Ok(Blake3Hash::from_bytes(&contents))
}

/// Create a deterministic tarball of a directory and return its hash
///
/// This creates a reproducible tarball by:
/// - Sorting entries
/// - Setting fixed mtime/uid/gid
/// - Using consistent permissions
pub fn hash_directory_as_tar(dir: &Utf8Path) -> Result<(Vec<u8>, Blake3Hash), ZigError> {
    let mut builder = tar::Builder::new(Vec::new());

    // Collect entries and sort them for determinism
    let mut entries: Vec<_> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .collect();

    entries.sort_by(|a, b| a.path().cmp(b.path()));

    for entry in entries {
        let path = entry.path();
        let rel_path = path
            .strip_prefix(dir)
            .map_err(|e| ZigError::ExtractionFailed(format!("path prefix error: {}", e)))?;

        if rel_path.as_os_str().is_empty() {
            continue; // Skip the root directory itself
        }

        let metadata = entry
            .metadata()
            .map_err(|e| ZigError::ExtractionFailed(format!("failed to read metadata: {}", e)))?;

        let mut header = tar::Header::new_gnu();
        header.set_mtime(0); // Fixed mtime for reproducibility
        header.set_uid(0);
        header.set_gid(0);

        if metadata.is_file() {
            let contents = std::fs::read(path)?;
            header.set_size(contents.len() as u64);
            header.set_mode(if is_executable(&metadata) {
                0o755
            } else {
                0o644
            });
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();

            builder
                .append_data(&mut header, rel_path, contents.as_slice())
                .map_err(|e| {
                    ZigError::ExtractionFailed(format!("failed to add file to tar: {}", e))
                })?;
        } else if metadata.is_dir() {
            header.set_size(0);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_cksum();

            builder
                .append_data(&mut header, rel_path, std::io::empty())
                .map_err(|e| {
                    ZigError::ExtractionFailed(format!("failed to add dir to tar: {}", e))
                })?;
        } else if metadata.is_symlink() {
            let target = std::fs::read_link(path)?;
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);

            let target_str = target.to_string_lossy();
            builder
                .append_link(&mut header, rel_path, target_str.as_ref())
                .map_err(|e| {
                    ZigError::ExtractionFailed(format!("failed to add symlink to tar: {}", e))
                })?;
        }
    }

    let tarball = builder
        .into_inner()
        .map_err(|e| ZigError::ExtractionFailed(format!("failed to finish tar: {}", e)))?;

    let hash = Blake3Hash::from_bytes(&tarball);

    Ok((tarball, hash))
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Acquire a Zig toolchain: download, extract, hash, and return metadata
///
/// This function:
/// 1. Downloads the Zig tarball from ziglang.org
/// 2. Extracts it to a temporary directory
/// 3. Hashes the zig executable and lib directory
/// 4. Returns the toolchain metadata (caller is responsible for CAS storage)
pub async fn acquire_zig_toolchain(
    version: &ZigVersion,
    platform: &HostPlatform,
    temp_dir: &Utf8Path,
) -> Result<AcquiredToolchain, ZigError> {
    // Download the tarball
    let tarball = download_zig(version, platform).await?;

    // Extract it
    let extract_dir = temp_dir.join(format!("zig-extract-{}", version));
    let (zig_exe_path, lib_dir_path) = extract_zig_tarball(&tarball, &extract_dir)?;

    // Hash the zig executable
    let zig_exe_contents = std::fs::read(&zig_exe_path)?;
    let zig_exe_hash = Blake3Hash::from_bytes(&zig_exe_contents);

    tracing::debug!(
        hash = %zig_exe_hash,
        size = zig_exe_contents.len(),
        "hashed zig executable"
    );

    // Hash the lib directory as a tarball
    let (lib_tarball, lib_hash) = hash_directory_as_tar(&lib_dir_path)?;

    tracing::debug!(
        hash = %lib_hash,
        size = lib_tarball.len(),
        "hashed zig lib directory"
    );

    // Compute toolchain ID
    let toolchain_id = ToolchainId::from_parts(&zig_exe_hash, &lib_hash);

    tracing::info!(
        toolchain_id = %toolchain_id,
        version = %version,
        platform = %platform,
        "acquired zig toolchain"
    );

    Ok(AcquiredToolchain {
        id: toolchain_id,
        version: version.clone(),
        host: platform.clone(),
        zig_exe_hash,
        zig_exe_contents,
        lib_hash,
        lib_tarball,
        extract_dir,
    })
}

/// Result of acquiring a Zig toolchain (before CAS storage)
pub struct AcquiredToolchain {
    /// Toolchain identifier
    pub id: ToolchainId,
    /// Zig version
    pub version: ZigVersion,
    /// Host platform
    pub host: HostPlatform,
    /// Hash of the zig executable
    pub zig_exe_hash: Blake3Hash,
    /// Contents of the zig executable (for CAS storage)
    pub zig_exe_contents: Vec<u8>,
    /// Hash of the lib directory tarball
    pub lib_hash: Blake3Hash,
    /// Lib directory tarball (for CAS storage)
    pub lib_tarball: Vec<u8>,
    /// Extraction directory (for cleanup)
    pub extract_dir: Utf8PathBuf,
}

impl AcquiredToolchain {
    /// Create a ZigToolchain from this acquired toolchain
    pub fn into_toolchain(self) -> ZigToolchain {
        ZigToolchain {
            id: self.id,
            version: self.version,
            host: self.host,
            zig_exe_hash: self.zig_exe_hash,
            lib_hash: self.lib_hash,
        }
    }

    /// Clean up the extraction directory
    pub fn cleanup(&self) -> Result<(), ZigError> {
        if self.extract_dir.exists() {
            std::fs::remove_dir_all(&self.extract_dir)?;
        }
        Ok(())
    }
}

/// Materialize a Zig toolchain from CAS to a local directory
///
/// Given the zig executable and lib tarball from CAS, extract them to a directory
/// suitable for running zig commands.
pub fn materialize_toolchain(
    zig_exe: &[u8],
    lib_tarball: &[u8],
    target_dir: &Utf8Path,
) -> Result<MaterializedToolchain, ZigError> {
    use std::fs;

    // Create target directory
    fs::create_dir_all(target_dir)?;

    // Write zig executable
    let zig_path = target_dir.join("zig");
    let mut file = fs::File::create(&zig_path)?;
    file.write_all(zig_exe)?;

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&zig_path, fs::Permissions::from_mode(0o755))?;
    }

    // Extract lib tarball
    let lib_dir = target_dir.join("lib");
    fs::create_dir_all(&lib_dir)?;

    let mut archive = tar::Archive::new(lib_tarball);
    archive
        .unpack(&lib_dir)
        .map_err(|e| ZigError::ExtractionFailed(format!("failed to unpack lib: {}", e)))?;

    Ok(MaterializedToolchain { zig_path, lib_dir })
}

/// A toolchain materialized to the filesystem, ready for use
pub struct MaterializedToolchain {
    /// Path to the zig executable
    pub zig_path: Utf8PathBuf,
    /// Path to the lib directory
    pub lib_dir: Utf8PathBuf,
}

impl MaterializedToolchain {
    /// Get the path to use for `zig cc` commands
    pub fn cc_path(&self) -> &Utf8Path {
        &self.zig_path
    }

    /// Build command-line arguments for compiling a C file
    pub fn cc_compile_args(
        &self,
        source: &Utf8Path,
        output: &Utf8Path,
        target_triple: &str,
    ) -> Vec<String> {
        vec![
            "cc".to_string(),
            "-target".to_string(),
            target_triple.to_string(),
            "-c".to_string(),
            source.to_string(),
            "-o".to_string(),
            output.to_string(),
        ]
    }

    /// Build command-line arguments for linking object files
    pub fn cc_link_args(
        &self,
        objects: &[&Utf8Path],
        output: &Utf8Path,
        target_triple: &str,
    ) -> Vec<String> {
        let mut args = vec![
            "cc".to_string(),
            "-target".to_string(),
            target_triple.to_string(),
        ];

        for obj in objects {
            args.push(obj.to_string());
        }

        args.push("-o".to_string());
        args.push(output.to_string());

        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zig_version_display() {
        let v = ZigVersion::new("0.13.0");
        assert_eq!(v.to_string(), "0.13.0");
        assert!(v.is_release());

        let v_dev = ZigVersion::new("0.14.0-dev.1234+abcdef");
        assert!(!v_dev.is_release());
    }

    #[test]
    fn zig_download_url_linux() {
        let v = ZigVersion::new("0.13.0");
        let p = HostPlatform {
            os: "linux".into(),
            arch: "x86_64".into(),
        };

        assert_eq!(
            zig_download_url(&v, &p),
            "https://ziglang.org/download/0.13.0/zig-x86_64-linux-0.13.0.tar.xz"
        );
    }

    #[test]
    fn zig_download_url_macos() {
        let v = ZigVersion::new("0.13.0");
        let p = HostPlatform {
            os: "macos".into(),
            arch: "aarch64".into(),
        };

        assert_eq!(
            zig_download_url(&v, &p),
            "https://ziglang.org/download/0.13.0/zig-aarch64-macos-0.13.0.tar.xz"
        );
    }

    #[test]
    fn toolchain_id_deterministic() {
        let exe_hash = Blake3Hash::from_bytes(b"zig executable contents");
        let lib_hash = Blake3Hash::from_bytes(b"lib tarball contents");

        let id1 = ToolchainId::from_parts(&exe_hash, &lib_hash);
        let id2 = ToolchainId::from_parts(&exe_hash, &lib_hash);

        assert_eq!(id1, id2);

        // Different content = different ID
        let lib_hash2 = Blake3Hash::from_bytes(b"different lib contents");
        let id3 = ToolchainId::from_parts(&exe_hash, &lib_hash2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn host_platform_detection() {
        // This just tests that detection works on the current platform
        let platform = HostPlatform::detect().unwrap();
        assert!(!platform.os.is_empty());
        assert!(!platform.arch.is_empty());
    }

    #[tokio::test]
    #[ignore] // Requires network access - run with --ignored
    async fn download_and_extract_zig() {
        let version = ZigVersion::new("0.13.0");
        let platform = HostPlatform::detect().unwrap();

        let temp_dir = Utf8PathBuf::from("/tmp/vx-zig-test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let acquired = acquire_zig_toolchain(&version, &platform, &temp_dir)
            .await
            .unwrap();

        println!("Toolchain ID: {}", acquired.id);
        println!("Zig exe hash: {}", acquired.zig_exe_hash);
        println!("Lib hash: {}", acquired.lib_hash);

        // Verify we can materialize it
        let materialize_dir = temp_dir.join("materialized");
        let materialized = materialize_toolchain(
            &acquired.zig_exe_contents,
            &acquired.lib_tarball,
            &materialize_dir,
        )
        .unwrap();

        assert!(materialized.zig_path.exists());
        assert!(materialized.lib_dir.exists());

        // Cleanup
        acquired.cleanup().unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
