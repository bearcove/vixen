//! Toolchain management for vx
//!
//! Downloads and manages toolchains for hermetic builds:
//! - Rust toolchains from static.rust-lang.org (without rustup)
//! - Zig toolchains for C/C++ compilation (via `zig cc`)
//!
//! Toolchains are stored in CAS for deduplication and remote execution.

pub mod zig;

use std::collections::HashMap;

use facet::Facet;
use facet_value::Value;
use thiserror::Error;

/// Errors that can occur during toolchain operations
#[derive(Debug, Error)]
pub enum ToolchainError {
    #[error("failed to fetch manifest from {url}: {source}")]
    FetchError {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to parse channel manifest: {0}")]
    ParseError(String),

    #[error("component {component} not available for target {target}")]
    ComponentNotAvailable { component: String, target: String },

    #[error("invalid channel specification: {0}")]
    InvalidChannel(String),

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Rust channel specification (what the user specifies)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Stable channel (latest stable)
    Stable,
    /// Beta channel
    Beta,
    /// Nightly channel with date pin (e.g., "2024-02-01")
    Nightly { date: String },
}

impl Channel {
    /// Parse a channel string like "stable", "beta", or "nightly-2024-02-01"
    pub fn parse(s: &str) -> Result<Self, ToolchainError> {
        match s {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            s if s.starts_with("nightly-") => {
                let date = s.strip_prefix("nightly-").unwrap();
                // Validate date format (YYYY-MM-DD)
                if date.len() != 10
                    || !date
                        .chars()
                        .enumerate()
                        .all(|(i, c)| (i == 4 || i == 7 && c == '-') || c.is_ascii_digit())
                {
                    return Err(ToolchainError::InvalidChannel(format!(
                        "nightly date must be YYYY-MM-DD, got: {}",
                        date
                    )));
                }
                Ok(Channel::Nightly {
                    date: date.to_string(),
                })
            }
            _ => Err(ToolchainError::InvalidChannel(format!(
                "unknown channel: {}",
                s
            ))),
        }
    }

    /// Get the manifest URL for this channel
    pub fn manifest_url(&self) -> String {
        match self {
            Channel::Stable => {
                "https://static.rust-lang.org/dist/channel-rust-stable.toml".to_string()
            }
            Channel::Beta => "https://static.rust-lang.org/dist/channel-rust-beta.toml".to_string(),
            Channel::Nightly { date } => {
                format!(
                    "https://static.rust-lang.org/dist/{}/channel-rust-nightly.toml",
                    date
                )
            }
        }
    }
}

/// Parsed channel manifest from static.rust-lang.org
#[derive(Debug, Clone)]
pub struct ChannelManifest {
    /// Release date (e.g., "2025-12-11")
    pub date: String,
    /// Rustc package metadata
    pub rustc: PackageManifest,
    /// Rust-std package metadata
    pub rust_std: PackageManifest,
}

/// Package metadata from the channel manifest
#[derive(Debug, Clone)]
pub struct PackageManifest {
    /// Package version (e.g., "1.76.0")
    pub version: String,
    /// Git commit hash
    pub git_commit_hash: Option<String>,
    /// Per-target download information
    pub targets: HashMap<String, TargetManifest>,
}

/// Per-target download information
#[derive(Debug, Clone)]
pub struct TargetManifest {
    /// Whether this target is available
    pub available: bool,
    /// Download URL (xz-compressed tarball)
    pub url: String,
    /// SHA256 hash of the tarball
    pub hash: String,
}

// =============================================================================
// RAW TOML STRUCTURES (for facet-toml parsing)
// =============================================================================

/// Raw channel manifest structure
#[derive(Facet, Debug)]
#[facet(rename_all = "kebab-case")]
struct RawChannelManifest {
    manifest_version: Option<String>,
    date: Option<String>,
    pkg: Option<Value>,
}

/// Individual package entry
#[derive(Facet, Debug)]
struct RawPackage {
    version: Option<String>,
    git_commit_hash: Option<String>,
    target: Option<Value>,
}

/// Target-specific download info
#[derive(Facet, Debug)]
struct RawTarget {
    available: Option<bool>,
    url: Option<String>,
    hash: Option<String>,
    xz_url: Option<String>,
    xz_hash: Option<String>,
    // Ignore components and extensions arrays (only present in some packages)
    components: Option<Value>,
    extensions: Option<Value>,
}

impl ChannelManifest {
    /// Parse a channel manifest from TOML content
    pub fn from_toml(contents: &str) -> Result<Self, ToolchainError> {
        let raw: RawChannelManifest = facet_toml::from_str(contents)
            .map_err(|e| ToolchainError::ParseError(e.to_string()))?;

        let date = raw
            .date
            .ok_or_else(|| ToolchainError::ParseError("missing 'date' field".into()))?;

        let pkg_value = raw
            .pkg
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg' table".into()))?;

        // Extract rustc and rust-std from the pkg object
        let pkg_obj = pkg_value
            .as_object()
            .ok_or_else(|| ToolchainError::ParseError("pkg is not an object".into()))?;

        let rustc_value = pkg_obj
            .get("rustc")
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rustc'".into()))?;
        let rustc: RawPackage = facet_value::from_value(rustc_value.clone())
            .map_err(|e| ToolchainError::ParseError(format!("failed to parse rustc: {}", e)))?;

        let rust_std_value = pkg_obj
            .get("rust-std")
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rust-std'".into()))?;
        let rust_std: RawPackage = facet_value::from_value(rust_std_value.clone())
            .map_err(|e| ToolchainError::ParseError(format!("failed to parse rust-std: {}", e)))?;

        Ok(ChannelManifest {
            date,
            rustc: parse_package(rustc)?,
            rust_std: parse_package(rust_std)?,
        })
    }

    /// Get the download info for rustc for a specific target
    pub fn rustc_for_target(&self, target: &str) -> Result<&TargetManifest, ToolchainError> {
        self.rustc
            .targets
            .get(target)
            .filter(|t| t.available)
            .ok_or_else(|| ToolchainError::ComponentNotAvailable {
                component: "rustc".to_string(),
                target: target.to_string(),
            })
    }

    /// Get the download info for rust-std for a specific target
    pub fn rust_std_for_target(&self, target: &str) -> Result<&TargetManifest, ToolchainError> {
        self.rust_std
            .targets
            .get(target)
            .filter(|t| t.available)
            .ok_or_else(|| ToolchainError::ComponentNotAvailable {
                component: "rust-std".to_string(),
                target: target.to_string(),
            })
    }
}

fn parse_package(raw: RawPackage) -> Result<PackageManifest, ToolchainError> {
    let version = raw
        .version
        .ok_or_else(|| ToolchainError::ParseError("missing 'version' in package".into()))?;

    let mut targets = HashMap::new();

    if let Some(target_value) = raw.target {
        // The target field is an object/table containing platform triples as keys
        if let Some(obj) = target_value.as_object() {
            for (name, value) in obj.iter() {
                // Convert the Value back to RawTarget
                let raw_target: RawTarget =
                    facet_value::from_value(value.clone()).map_err(|e| {
                        ToolchainError::ParseError(format!(
                            "failed to parse target {}: {}",
                            name, e
                        ))
                    })?;

                let target = TargetManifest {
                    available: raw_target.available.unwrap_or(false),
                    // Prefer xz-compressed URLs
                    url: raw_target.xz_url.or(raw_target.url).unwrap_or_default(),
                    hash: raw_target.xz_hash.or(raw_target.hash).unwrap_or_default(),
                };
                targets.insert(name.to_string(), target);
            }
        }
    }

    Ok(PackageManifest {
        version,
        git_commit_hash: raw.git_commit_hash,
        targets,
    })
}

// =============================================================================
// DOWNLOAD FUNCTIONS
// =============================================================================

/// Fetch a channel manifest from static.rust-lang.org
pub async fn fetch_channel_manifest(channel: &Channel) -> Result<ChannelManifest, ToolchainError> {
    let url = channel.manifest_url();

    tracing::debug!(url = %url, "fetching channel manifest");

    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(ToolchainError::FetchError {
            url: url.clone(),
            source: format!("HTTP {}", response.status()).into(),
        });
    }

    let content = response.text().await?;
    ChannelManifest::from_toml(&content)
}

/// Download a component tarball and verify its checksum
pub async fn download_component(url: &str, expected_hash: &str) -> Result<Vec<u8>, ToolchainError> {
    tracing::info!(url = %url, "downloading component");

    let response = reqwest::get(url).await?;

    if !response.status().is_success() {
        return Err(ToolchainError::FetchError {
            url: url.to_string(),
            source: format!("HTTP {}", response.status()).into(),
        });
    }

    let bytes = response.bytes().await?;

    // Verify SHA256 checksum
    let hash = compute_sha256(&bytes);
    if hash != expected_hash {
        return Err(ToolchainError::ChecksumMismatch {
            url: url.to_string(),
            expected: expected_hash.to_string(),
            actual: hash,
        });
    }

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        hash = %hash,
        "component downloaded and verified"
    );

    Ok(bytes.to_vec())
}

/// Compute SHA256 hash of bytes and return as hex string
fn compute_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

// =============================================================================
// RUST TOOLCHAIN ACQUISITION
// =============================================================================

use camino::{Utf8Path, Utf8PathBuf};
use vx_cas_proto::Blake3Hash;

/// Rust toolchain specification
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RustToolchainSpec {
    /// Channel (stable, beta, or pinned nightly)
    pub channel: Channel,
    /// Host triple (e.g., "aarch64-apple-darwin")
    pub host: String,
    /// Target triple for cross-compilation (defaults to host if not set)
    pub target: Option<String>,
}

impl RustToolchainSpec {
    /// Create a spec for native compilation on the current host
    pub fn native(channel: Channel) -> Result<Self, ToolchainError> {
        let host = detect_host_triple()?;
        Ok(Self {
            channel,
            host,
            target: None,
        })
    }

    /// Create a spec for cross-compilation
    pub fn cross(channel: Channel, host: String, target: String) -> Self {
        Self {
            channel,
            host,
            target: Some(target),
        }
    }

    /// Get the effective target triple
    pub fn effective_target(&self) -> &str {
        self.target.as_deref().unwrap_or(&self.host)
    }
}

/// Unique identifier for a Rust toolchain, derived from manifest SHA256 checksums.
///
/// The toolchain ID is deterministic and derived from the *manifest-declared*
/// SHA256 hashes, NOT from the downloaded bytes. This means:
/// - Identity is defined by what Rust publishes (the manifest)
/// - Verification happens separately (downloaded bytes must match manifest SHA256)
/// - A corrupted download fails verification, it doesn't silently become a different toolchain
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RustToolchainId(pub Blake3Hash);

impl RustToolchainId {
    /// Create from manifest SHA256 hashes (not downloaded byte hashes!)
    ///
    /// Both hashes are the SHA256 hex strings from the channel manifest.
    pub fn from_manifest_sha256s(rustc_sha256: &str, rust_std_sha256: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rust-toolchain-v1\n");
        hasher.update(b"rustc_sha256:");
        hasher.update(rustc_sha256.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"rust_std_sha256:");
        hasher.update(rust_std_sha256.as_bytes());
        hasher.update(b"\n");
        Self(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    /// Get the hex representation
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }

    /// Get a short hex prefix (for display)
    pub fn short_hex(&self) -> String {
        self.0.short_hex()
    }
}

impl std::fmt::Display for RustToolchainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rust:{}", self.short_hex())
    }
}

/// Result of acquiring a Rust toolchain (before CAS storage)
pub struct AcquiredRustToolchain {
    /// Unique toolchain identifier (derived from manifest SHA256s)
    pub id: RustToolchainId,
    /// Toolchain specification
    pub spec: RustToolchainSpec,
    /// Rustc version string
    pub rustc_version: String,
    /// Rustc component tarball (xz compressed)
    pub rustc_tarball: Vec<u8>,
    /// Rustc manifest SHA256 (hex string, used for identity)
    pub rustc_manifest_sha256: String,
    /// Rust-std component tarball (xz compressed)
    pub rust_std_tarball: Vec<u8>,
    /// Rust-std manifest SHA256 (hex string, used for identity)
    pub rust_std_manifest_sha256: String,
    /// Channel manifest date
    pub manifest_date: String,
}

/// A materialized Rust toolchain ready for use
pub struct MaterializedRustToolchain {
    /// Root directory of the toolchain
    pub root: Utf8PathBuf,
    /// Path to the rustc binary
    pub rustc: Utf8PathBuf,
    /// Sysroot path (for --sysroot flag)
    pub sysroot: Utf8PathBuf,
}

impl MaterializedRustToolchain {
    /// Get rustc command arguments for using this toolchain
    pub fn rustc_args(&self) -> Vec<String> {
        vec![format!("--sysroot={}", self.sysroot)]
    }
}

/// Detect the host triple using rustc (fallback if no hermetic toolchain yet)
pub fn detect_host_triple() -> Result<String, ToolchainError> {
    use std::process::Command;

    let output =
        Command::new("rustc")
            .arg("-vV")
            .output()
            .map_err(|e| ToolchainError::FetchError {
                url: "rustc -vV".to_string(),
                source: Box::new(e),
            })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }

    Err(ToolchainError::FetchError {
        url: "rustc -vV".to_string(),
        source: "could not determine host triple".into(),
    })
}

/// Acquire a Rust toolchain: download components, verify checksums, compute ID
///
/// The toolchain ID is derived from the manifest SHA256 hashes (what Rust publishes),
/// not from the downloaded bytes. Verification ensures downloaded bytes match the
/// manifest - a mismatch fails loudly rather than creating a different toolchain ID.
pub async fn acquire_rust_toolchain(
    spec: &RustToolchainSpec,
) -> Result<AcquiredRustToolchain, ToolchainError> {
    tracing::info!(
        channel = ?spec.channel,
        host = %spec.host,
        target = ?spec.target,
        "acquiring Rust toolchain"
    );

    // Fetch the channel manifest
    let manifest = fetch_channel_manifest(&spec.channel).await?;

    tracing::debug!(
        date = %manifest.date,
        rustc_version = %manifest.rustc.version,
        "fetched channel manifest"
    );

    // Get manifest SHA256s - these define toolchain identity
    let rustc_target = manifest.rustc_for_target(&spec.host)?;
    let rustc_manifest_sha256 = rustc_target.hash.clone();

    let target = spec.effective_target();
    let rust_std_target = manifest.rust_std_for_target(target)?;
    let rust_std_manifest_sha256 = rust_std_target.hash.clone();

    // Compute toolchain ID from manifest SHA256s (before downloading)
    // This ensures ID is stable and defined by what Rust publishes
    let id =
        RustToolchainId::from_manifest_sha256s(&rustc_manifest_sha256, &rust_std_manifest_sha256);

    tracing::info!(
        toolchain_id = %id,
        rustc_version = %manifest.rustc.version,
        rustc_sha256 = %rustc_manifest_sha256,
        rust_std_sha256 = %rust_std_manifest_sha256,
        "toolchain identity computed from manifest"
    );

    // Download and verify components (verification happens inside download_component)
    // If verification fails, we get an error - not a different ID
    let rustc_tarball = download_component(&rustc_target.url, &rustc_manifest_sha256).await?;
    let rust_std_tarball =
        download_component(&rust_std_target.url, &rust_std_manifest_sha256).await?;

    tracing::info!(
        toolchain_id = %id,
        rustc_version = %manifest.rustc.version,
        "acquired Rust toolchain"
    );

    Ok(AcquiredRustToolchain {
        id,
        spec: spec.clone(),
        rustc_version: manifest.rustc.version,
        rustc_tarball,
        rustc_manifest_sha256,
        rust_std_tarball,
        rust_std_manifest_sha256,
        manifest_date: manifest.date,
    })
}

/// Extract and materialize a Rust toolchain to a directory
///
/// This unpacks the rustc and rust-std tarballs and sets up the sysroot
/// by directly copying files - NO install.sh execution for hermeticity.
///
/// Rust component tarballs have a known structure:
/// ```text
/// rustc-{version}-{target}/
///   rustc/
///     bin/rustc, bin/rustdoc, ...
///     lib/rustlib/{target}/...
///   ...
/// rust-std-{version}-{target}/
///   rust-std-{target}/
///     lib/rustlib/{target}/lib/...
/// ```
///
/// We copy the contents directly into a sysroot layout.
pub fn materialize_rust_toolchain(
    rustc_tarball: &[u8],
    rust_std_tarball: &[u8],
    target_dir: &Utf8Path,
) -> Result<MaterializedRustToolchain, ToolchainError> {
    use std::fs;

    tracing::debug!(target_dir = %target_dir, "materializing Rust toolchain");

    // Create target directory and sysroot
    let sysroot = target_dir.join("sysroot");
    fs::create_dir_all(&sysroot).map_err(ToolchainError::IoError)?;

    // Extract and install rustc component
    extract_and_install_component(rustc_tarball, "rustc", target_dir, &sysroot)?;

    // Extract and install rust-std component
    extract_and_install_component(rust_std_tarball, "rust-std", target_dir, &sysroot)?;

    // Find rustc binary
    let rustc = sysroot.join("bin/rustc");
    if !rustc.exists() {
        return Err(ToolchainError::FetchError {
            url: "rustc".to_string(),
            source: format!("rustc binary not found at {}", rustc).into(),
        });
    }

    // Make rustc executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&rustc)
            .map_err(ToolchainError::IoError)?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&rustc, perms).map_err(ToolchainError::IoError)?;
    }

    tracing::debug!(
        rustc = %rustc,
        sysroot = %sysroot,
        "materialized Rust toolchain"
    );

    Ok(MaterializedRustToolchain {
        root: target_dir.to_owned(),
        rustc,
        sysroot,
    })
}

/// Extract a component tarball and copy its contents to the sysroot.
///
/// This replaces install.sh execution with direct file copying for hermeticity.
fn extract_and_install_component(
    tarball: &[u8],
    component_name: &str,
    extract_dir: &Utf8Path,
    sysroot: &Utf8Path,
) -> Result<(), ToolchainError> {
    use std::fs;
    use std::io::Read;
    use xz2::read::XzDecoder;

    // Decompress xz
    let mut decoder = XzDecoder::new(tarball);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| ToolchainError::FetchError {
            url: format!("{} tarball", component_name),
            source: format!("xz decompression failed: {}", e).into(),
        })?;

    // Extract tar to temporary location
    let mut archive = tar::Archive::new(decompressed.as_slice());
    archive
        .unpack(extract_dir)
        .map_err(|e| ToolchainError::FetchError {
            url: format!("{} tarball", component_name),
            source: format!("tar extraction failed: {}", e).into(),
        })?;

    // Find the extracted component directory and copy its contents to sysroot
    // Rust tarballs extract to: {component}-{version}-{target}/{subcomponent}/
    for entry in fs::read_dir(extract_dir).map_err(ToolchainError::IoError)? {
        let entry = entry.map_err(ToolchainError::IoError)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Match directories like "rustc-1.76.0-aarch64-apple-darwin" or "rust-std-1.76.0-aarch64-apple-darwin"
        if name_str.starts_with(component_name) && entry.path().is_dir() {
            let component_root =
                Utf8PathBuf::try_from(entry.path()).map_err(|e| ToolchainError::FetchError {
                    url: component_name.to_string(),
                    source: format!("invalid path: {}", e).into(),
                })?;

            // Copy contents from each subdirectory (rustc/, rust-std-{target}/, etc.)
            install_component_contents(&component_root, sysroot)?;
        }
    }

    Ok(())
}

/// Copy component contents to sysroot, preserving directory structure.
///
/// Each component tarball contains subdirectories with the actual files:
/// - rustc tarball has: rustc/ (with bin/, lib/, etc.)
/// - rust-std tarball has: rust-std-{target}/ (with lib/rustlib/{target}/lib/)
fn install_component_contents(
    component_root: &Utf8Path,
    sysroot: &Utf8Path,
) -> Result<(), ToolchainError> {
    use std::fs;

    for entry in fs::read_dir(component_root).map_err(ToolchainError::IoError)? {
        let entry = entry.map_err(ToolchainError::IoError)?;
        let path = entry.path();

        // Skip non-directories and special files (install.sh, manifest.in, etc.)
        if !path.is_dir() {
            continue;
        }

        let subdir_name = entry.file_name();
        let subdir_name_str = subdir_name.to_string_lossy();

        // Skip directories that aren't component content
        if subdir_name_str == "." || subdir_name_str == ".." {
            continue;
        }

        let src = Utf8PathBuf::try_from(path).map_err(|e| ToolchainError::FetchError {
            url: component_root.to_string(),
            source: format!("invalid path: {}", e).into(),
        })?;

        // Copy the directory contents recursively to sysroot
        copy_dir_recursive(&src, sysroot)?;
    }

    Ok(())
}

/// Recursively copy a directory's contents to a destination, merging with existing.
fn copy_dir_recursive(src: &Utf8Path, dst: &Utf8Path) -> Result<(), ToolchainError> {
    use std::fs;

    for entry in fs::read_dir(src).map_err(ToolchainError::IoError)? {
        let entry = entry.map_err(ToolchainError::IoError)?;
        let src_path =
            Utf8PathBuf::try_from(entry.path()).map_err(|e| ToolchainError::FetchError {
                url: src.to_string(),
                source: format!("invalid path: {}", e).into(),
            })?;
        let file_name = src_path
            .file_name()
            .ok_or_else(|| ToolchainError::FetchError {
                url: src_path.to_string(),
                source: "path has no filename".into(),
            })?;
        let dst_path = dst.join(file_name);

        if src_path.is_dir() {
            fs::create_dir_all(&dst_path).map_err(ToolchainError::IoError)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            // Copy file, preserving permissions
            fs::copy(&src_path, &dst_path).map_err(ToolchainError::IoError)?;

            // Preserve executable bit on unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let src_meta = fs::metadata(&src_path).map_err(ToolchainError::IoError)?;
                let src_mode = src_meta.permissions().mode();
                if src_mode & 0o111 != 0 {
                    // Source is executable, make dest executable too
                    let mut perms = fs::metadata(&dst_path)
                        .map_err(ToolchainError::IoError)?
                        .permissions();
                    perms.set_mode(src_mode);
                    fs::set_permissions(&dst_path, perms).map_err(ToolchainError::IoError)?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
